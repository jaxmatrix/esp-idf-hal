//! SPI peripheral control
//!
//! Currently only implements full duplex controller mode support.
//!
//! SPI0 is reserved for accessing flash and sram and therefore not usable for other purposes.
//! SPI1 shares its external pins with SPI0 and therefore has severe restrictions in use.
//!
//! SPI2 & 3 can be used freely.
//!
//! The CS pin is controlled by hardware on esp32 (contrary to the description of embedded_hal).
//!
//! The [Transfer::transfer], [Write::write] and [WriteIter::write_iter] functions lock the
//! APB frequency and therefore the requests are always run at the requested baudrate.
//! The primitive [FullDuplex::read] and [FullDuplex::send] do not lock the APB frequency and
//! therefore may run at a different frequency.
//!
//! # TODO
//! - Quad SPI
//! - DMA
//! - Multiple CS pins
//! - Slave

use core::cmp::{max, min, Ordering};
use core::marker::PhantomData;
use core::ptr;

use embedded_hal::spi::blocking::{SpiBus, SpiBusFlush, SpiBusRead, SpiBusWrite, SpiDevice};

use esp_idf_sys::*;

use crate::delay::BLOCK;
use crate::gpio::{self, InputPin, OutputPin};
use crate::peripheral::{Peripheral, PeripheralRef};

crate::embedded_hal_error!(
    SpiError,
    embedded_hal::spi::Error,
    embedded_hal::spi::ErrorKind
);

pub trait Spi: Send {
    fn device() -> spi_host_device_t;
}

/// A marker interface implemented by all SPI peripherals except SPI1 which
/// should use a fixed set of pins
pub trait SpiAnyPins: Spi {}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Dma {
    Disabled,
    Channel1(usize),
    Channel2(usize),
    Auto(usize),
}

impl From<Dma> for spi_dma_chan_t {
    fn from(dma: Dma) -> Self {
        match dma {
            Dma::Channel1(_) => 1,
            Dma::Channel2(_) => 2,
            Dma::Auto(_) => 3,
            _ => 0,
        }
    }
}

impl Dma {
    const fn max_transfer_size(&self) -> usize {
        let max_transfer_size = match self {
            Dma::Disabled => TRANS_LEN,
            Dma::Channel1(size) | Dma::Channel2(size) | Dma::Auto(size) => *size,
        };
        if max_transfer_size % 4 != 0 {
            panic!("The max transfer size must a multiple of 4")
        } else if max_transfer_size > 4096 {
            4096
        } else {
            max_transfer_size
        }
    }
}

pub type SpiMasterConfig = config::Config;

/// SPI configuration
pub mod config {
    use crate::spi::Dma;
    use crate::units::*;

    pub struct V02Type<T>(pub T);

    impl From<V02Type<embedded_hal_0_2::spi::Polarity>> for embedded_hal::spi::Polarity {
        fn from(polarity: V02Type<embedded_hal_0_2::spi::Polarity>) -> Self {
            match polarity.0 {
                embedded_hal_0_2::spi::Polarity::IdleHigh => embedded_hal::spi::Polarity::IdleHigh,
                embedded_hal_0_2::spi::Polarity::IdleLow => embedded_hal::spi::Polarity::IdleLow,
            }
        }
    }

    impl From<V02Type<embedded_hal_0_2::spi::Phase>> for embedded_hal::spi::Phase {
        fn from(phase: V02Type<embedded_hal_0_2::spi::Phase>) -> Self {
            match phase.0 {
                embedded_hal_0_2::spi::Phase::CaptureOnFirstTransition => {
                    embedded_hal::spi::Phase::CaptureOnFirstTransition
                }
                embedded_hal_0_2::spi::Phase::CaptureOnSecondTransition => {
                    embedded_hal::spi::Phase::CaptureOnSecondTransition
                }
            }
        }
    }

    impl From<V02Type<embedded_hal_0_2::spi::Mode>> for embedded_hal::spi::Mode {
        fn from(mode: V02Type<embedded_hal_0_2::spi::Mode>) -> Self {
            Self {
                polarity: V02Type(mode.0.polarity).into(),
                phase: V02Type(mode.0.phase).into(),
            }
        }
    }

    /// SPI configuration
    #[derive(Copy, Clone)]
    pub struct Config {
        pub baudrate: Hertz,
        pub data_mode: embedded_hal::spi::Mode,
        /// This property can be set to configure a SPI Device for being write only
        /// Thus the flag SPI_DEVICE_NO_DUMMY will be passed on initialization and
        /// it will unlock the possibility of using 80Mhz as the bus freq
        /// See https://docs.espressif.com/projects/esp-idf/en/latest/esp32/api-reference/peripherals/spi_master.html#timing-considerations
        pub write_only: bool,
        pub dma: Dma,
    }

    impl Config {
        pub fn new() -> Self {
            Default::default()
        }

        #[must_use]
        pub fn baudrate(mut self, baudrate: Hertz) -> Self {
            self.baudrate = baudrate;
            self
        }

        #[must_use]
        pub fn data_mode(mut self, data_mode: embedded_hal::spi::Mode) -> Self {
            self.data_mode = data_mode;
            self
        }

        pub fn write_only(mut self, write_only: bool) -> Self {
            self.write_only = write_only;
            self
        }

        pub fn dma(mut self, dma: Dma) -> Self {
            self.dma = dma;
            self
        }
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                baudrate: Hertz(1_000_000),
                data_mode: embedded_hal::spi::MODE_0,
                write_only: false,
                dma: Dma::Disabled,
            }
        }
    }
}

pub struct SpiBusMasterDriver<'d> {
    handle: spi_device_handle_t,
    trans_len: usize,
    _p: PhantomData<&'d ()>,
}

impl<'d> SpiBusMasterDriver<'d> {
    pub fn read(&mut self, words: &mut [u8]) -> Result<(), EspError> {
        for chunk in words.chunks_mut(self.trans_len) {
            self.polling_transmit(chunk.as_mut_ptr(), ptr::null(), chunk.len(), chunk.len())?;
        }

        Ok(())
    }

    pub fn write(&mut self, words: &[u8]) -> Result<(), EspError> {
        for chunk in words.chunks(self.trans_len) {
            self.polling_transmit(ptr::null_mut(), chunk.as_ptr(), chunk.len(), 0)?;
        }

        Ok(())
    }

    pub fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), EspError> {
        let common_length = min(read.len(), write.len());
        let common_read = read[0..common_length].chunks_mut(self.trans_len);
        let common_write = write[0..common_length].chunks(self.trans_len);

        for (read_chunk, write_chunk) in common_read.zip(common_write) {
            self.polling_transmit(
                read_chunk.as_mut_ptr(),
                write_chunk.as_ptr(),
                max(read_chunk.len(), write_chunk.len()),
                read_chunk.len(),
            )?;
        }

        match read.len().cmp(&write.len()) {
            Ordering::Equal => { /* Nothing left to do */ }
            Ordering::Greater => {
                // Read remainder
                self.read(&mut read[write.len()..])?;
            }
            Ordering::Less => {
                // Write remainder
                self.write(&write[read.len()..])?;
            }
        }

        Ok(())
    }

    pub fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), EspError> {
        for chunk in words.chunks_mut(self.trans_len) {
            let ptr = chunk.as_mut_ptr();
            let len = chunk.len();
            self.polling_transmit(ptr, ptr, len, len)?;
        }

        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), EspError> {
        // Since we use polling transactions, flushing isn't required.
        // In future, when DMA is available spi_device_get_trans_result
        // will be called here.
        Ok(())
    }

    fn polling_transmit(
        &mut self,
        read: *mut u8,
        write: *const u8,
        transaction_length: usize,
        rx_length: usize,
    ) -> Result<(), EspError> {
        polling_transmit(
            self.handle,
            read,
            write,
            transaction_length,
            rx_length,
            true,
        )
    }

    /// Empty transaction to de-assert CS.
    fn finish(&mut self) -> Result<(), EspError> {
        polling_transmit(self.handle, ptr::null_mut(), ptr::null(), 0, 0, false)
    }
}

impl<'d> embedded_hal::spi::ErrorType for SpiBusMasterDriver<'d> {
    type Error = SpiError;
}

impl<'d> SpiBusFlush for SpiBusMasterDriver<'d> {
    fn flush(&mut self) -> Result<(), Self::Error> {
        SpiBusMasterDriver::flush(self).map_err(to_spi_err)
    }
}

impl<'d> SpiBusRead for SpiBusMasterDriver<'d> {
    fn read(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        SpiBusMasterDriver::read(self, words).map_err(to_spi_err)
    }
}

impl<'d> SpiBusWrite for SpiBusMasterDriver<'d> {
    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        SpiBusMasterDriver::write(self, words).map_err(to_spi_err)
    }
}

impl<'d> SpiBus for SpiBusMasterDriver<'d> {
    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        SpiBusMasterDriver::transfer(self, read, write).map_err(to_spi_err)
    }

    fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        SpiBusMasterDriver::transfer_in_place(self, words).map_err(to_spi_err)
    }
}

/// Master SPI abstraction
pub struct SpiMasterDriver<'d, SPI: Spi> {
    _spi: PeripheralRef<'d, SPI>,
    device: spi_device_handle_t,
    max_transfer_size: usize,
}

impl<'d> SpiMasterDriver<'d, SPI1> {
    /// Create new instance of SPI controller for SPI1
    ///
    /// SPI1 can only use fixed pin for SCLK, SDO and SDI as they are shared with SPI0.
    pub fn new_spi1(
        spi: impl Peripheral<P = SPI1> + 'd,
        sclk: impl Peripheral<P = gpio::Gpio6> + 'd,
        sdo: impl Peripheral<P = gpio::Gpio7> + 'd,
        sdi: Option<impl Peripheral<P = gpio::Gpio8> + 'd>,
        cs: Option<impl Peripheral<P = impl OutputPin> + 'd>,
        config: &config::Config,
    ) -> Result<Self, EspError> {
        SpiMasterDriver::new_internal(spi, sclk, sdo, sdi, cs, config)
    }
}

impl<'d, SPI: SpiAnyPins> SpiMasterDriver<'d, SPI> {
    /// Create new instance of SPI controller for all others
    pub fn new(
        spi: impl Peripheral<P = SPI> + 'd,
        sclk: impl Peripheral<P = impl OutputPin> + 'd,
        sdo: impl Peripheral<P = impl OutputPin> + 'd,
        sdi: Option<impl Peripheral<P = impl InputPin + OutputPin> + 'd>,
        cs: Option<impl Peripheral<P = impl OutputPin> + 'd>,
        config: &config::Config,
    ) -> Result<Self, EspError> {
        SpiMasterDriver::new_internal(spi, sclk, sdo, sdi, cs, config)
    }
}

impl<'d, SPI: Spi> SpiMasterDriver<'d, SPI> {
    /// Internal implementation of new shared by all SPI controllers
    fn new_internal(
        spi: impl Peripheral<P = SPI> + 'd,
        sclk: impl Peripheral<P = impl OutputPin> + 'd,
        sdo: impl Peripheral<P = impl OutputPin> + 'd,
        sdi: Option<impl Peripheral<P = impl InputPin + OutputPin> + 'd>,
        cs: Option<impl Peripheral<P = impl OutputPin> + 'd>,
        config: &config::Config,
    ) -> Result<Self, EspError> {
        crate::into_ref!(spi, sclk, sdo);

        let sdi = sdi.map(|sdi| sdi.into_ref());
        let cs = cs.map(|cs| cs.into_ref());

        #[cfg(not(esp_idf_version = "4.3"))]
        let bus_config = spi_bus_config_t {
            flags: SPICOMMON_BUSFLAG_MASTER,
            sclk_io_num: sclk.pin(),

            data4_io_num: -1,
            data5_io_num: -1,
            data6_io_num: -1,
            data7_io_num: -1,
            __bindgen_anon_1: spi_bus_config_t__bindgen_ty_1 {
                mosi_io_num: sdo.pin(),
                //data0_io_num: -1,
            },
            __bindgen_anon_2: spi_bus_config_t__bindgen_ty_2 {
                miso_io_num: sdi.as_ref().map_or(-1, |p| p.pin()),
                //data1_io_num: -1,
            },
            __bindgen_anon_3: spi_bus_config_t__bindgen_ty_3 {
                quadwp_io_num: -1,
                //data2_io_num: -1,
            },
            __bindgen_anon_4: spi_bus_config_t__bindgen_ty_4 {
                quadhd_io_num: -1,
                //data3_io_num: -1,
            },
            max_transfer_sz: config.dma.max_transfer_size() as i32,
            ..Default::default()
        };

        #[cfg(esp_idf_version = "4.3")]
        let bus_config = spi_bus_config_t {
            flags: SPICOMMON_BUSFLAG_MASTER,
            sclk_io_num: sclk.pin(),

            mosi_io_num: sdo.pin(),
            miso_io_num: sdi.as_ref().map_or(-1, |p| p.pin()),
            quadwp_io_num: -1,
            quadhd_io_num: -1,

            max_transfer_sz: config.dma.max_transfer_size() as i32,
            ..Default::default()
        };

        esp!(unsafe { spi_bus_initialize(SPI::device(), &bus_config, config.dma.into()) })?;

        let device_config = spi_device_interface_config_t {
            spics_io_num: cs.as_ref().map_or(-1, |p| p.pin()),
            clock_speed_hz: config.baudrate.0 as i32,
            mode: (((config.data_mode.polarity == embedded_hal::spi::Polarity::IdleHigh) as u8)
                << 1)
                | ((config.data_mode.phase == embedded_hal::spi::Phase::CaptureOnSecondTransition)
                    as u8),
            queue_size: 64,
            flags: if config.write_only {
                SPI_DEVICE_NO_DUMMY
            } else {
                0_u32
            },
            ..Default::default()
        };

        let mut device_handle: spi_device_handle_t = ptr::null_mut();

        esp!(unsafe {
            spi_bus_add_device(SPI::device(), &device_config, &mut device_handle as *mut _)
        })?;

        Ok(Self {
            _spi: spi,
            device: device_handle,
            max_transfer_size: config.dma.max_transfer_size(),
        })
    }

    pub fn device_handle(&mut self) -> spi_device_handle_t {
        self.device
    }

    pub fn transaction<R, E>(
        &mut self,
        f: impl FnOnce(&mut SpiBusMasterDriver<'d>) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<EspError>,
    {
        let mut bus = SpiBusMasterDriver {
            handle: self.device,
            trans_len: self.max_transfer_size,
            _p: PhantomData,
        };

        let lock = self.lock_bus()?;

        let trans_result = f(&mut bus);

        let finish_result = bus.finish();

        // Flush whatever is pending.
        // Note that this is done even when an error is returned from the transaction.
        let flush_result = bus.flush();

        core::mem::drop(lock);

        let result = trans_result?;
        finish_result?;
        flush_result?;

        Ok(result)
    }

    fn lock_bus(&mut self) -> Result<Lock, EspError> {
        Lock::new(self.device)
    }
}

impl<'d, SPI: Spi> Drop for SpiMasterDriver<'d, SPI> {
    fn drop(&mut self) {
        esp!(unsafe { spi_bus_remove_device(self.device) }).unwrap();
        esp!(unsafe { spi_bus_free(SPI::device()) }).unwrap();
    }
}

unsafe impl<'d, SPI: Spi> Send for SpiMasterDriver<'d, SPI> {}

impl<'d, SPI: Spi> embedded_hal::spi::ErrorType for SpiMasterDriver<'d, SPI> {
    type Error = SpiError;
}

impl<'d, SPI: Spi> SpiDevice for SpiMasterDriver<'d, SPI> {
    type Bus = SpiBusMasterDriver<'d>;

    fn transaction<R>(
        &mut self,
        f: impl FnOnce(&mut Self::Bus) -> Result<R, <Self::Bus as embedded_hal::spi::ErrorType>::Error>,
    ) -> Result<R, Self::Error> {
        SpiMasterDriver::transaction(self, f)
    }
}

impl<'d, SPI: Spi> embedded_hal_0_2::blocking::spi::Transfer<u8> for SpiMasterDriver<'d, SPI> {
    type Error = SpiError;

    fn transfer<'w>(&mut self, words: &'w mut [u8]) -> Result<&'w [u8], Self::Error> {
        let _lock = self.lock_bus();
        let mut chunks = words.chunks_mut(self.max_transfer_size).peekable();

        while let Some(chunk) = chunks.next() {
            let ptr = chunk.as_mut_ptr();
            let len = chunk.len();
            polling_transmit(self.device, ptr, ptr, len, len, chunks.peek().is_some())?;
        }

        Ok(words)
    }
}

impl<'d, SPI: Spi> embedded_hal_0_2::blocking::spi::Write<u8> for SpiMasterDriver<'d, SPI> {
    type Error = SpiError;

    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        let _lock = self.lock_bus();
        let mut chunks = words.chunks(self.max_transfer_size).peekable();

        while let Some(chunk) = chunks.next() {
            polling_transmit(
                self.device,
                ptr::null_mut(),
                chunk.as_ptr(),
                chunk.len(),
                0,
                chunks.peek().is_some(),
            )?;
        }

        Ok(())
    }
}

impl<'d, SPI: Spi> embedded_hal_0_2::blocking::spi::WriteIter<u8> for SpiMasterDriver<'d, SPI> {
    type Error = SpiError;

    fn write_iter<WI>(&mut self, words: WI) -> Result<(), Self::Error>
    where
        WI: IntoIterator<Item = u8>,
    {
        let mut words = words.into_iter();
        let mut buf = [0_u8; TRANS_LEN];

        self.transaction(|bus| {
            loop {
                let mut offset = 0_usize;

                while offset < buf.len() {
                    if let Some(word) = words.next() {
                        buf[offset] = word;
                        offset += 1;
                    } else {
                        break;
                    }
                }

                if offset == 0 {
                    break;
                }

                bus.write(&buf[..offset])?;
            }

            Ok(())
        })
    }
}

impl<'d, SPI: Spi> embedded_hal_0_2::blocking::spi::Transactional<u8> for SpiMasterDriver<'d, SPI> {
    type Error = SpiError;

    fn exec<'a>(
        &mut self,
        operations: &mut [embedded_hal_0_2::blocking::spi::Operation<'a, u8>],
    ) -> Result<(), Self::Error> {
        self.transaction(|bus| {
            for operation in operations {
                match operation {
                    embedded_hal_0_2::blocking::spi::Operation::Write(write) => bus.write(write),
                    embedded_hal_0_2::blocking::spi::Operation::Transfer(words) => {
                        bus.transfer_in_place(words)
                    }
                }?;
            }

            Ok(())
        })
    }
}

fn to_spi_err(err: EspError) -> SpiError {
    SpiError::other(err)
}

// Limit to 64, as we sometimes allocate a buffer of size TRANS_LEN on the stack, so we have to keep it small
// SOC_SPI_MAXIMUM_BUFFER_SIZE equals 64 or 72 (esp32s2) anyway
const TRANS_LEN: usize = if SOC_SPI_MAXIMUM_BUFFER_SIZE < 64_u32 {
    SOC_SPI_MAXIMUM_BUFFER_SIZE as _
} else {
    64_usize
};

struct Lock(spi_device_handle_t);

impl Lock {
    fn new(device: spi_device_handle_t) -> Result<Self, EspError> {
        esp!(unsafe { spi_device_acquire_bus(device, BLOCK) })?;

        Ok(Self(device))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            spi_device_release_bus(self.0);
        }
    }
}

// These parameters assume full duplex.
fn polling_transmit(
    handle: spi_device_handle_t,
    read: *mut u8,
    write: *const u8,
    transaction_length: usize,
    rx_length: usize,
    _keep_cs_active: bool,
) -> Result<(), EspError> {
    #[cfg(esp_idf_version = "4.3")]
    let flags = 0;

    // This unfortunately means that this implementation is incorrect for esp-idf < 4.4.
    // The CS pin should be kept active through transactions.
    #[cfg(not(esp_idf_version = "4.3"))]
    let flags = if _keep_cs_active {
        SPI_TRANS_CS_KEEP_ACTIVE
    } else {
        0
    };

    let mut transaction = spi_transaction_t {
        flags,
        __bindgen_anon_1: spi_transaction_t__bindgen_ty_1 {
            tx_buffer: write as *const _,
        },
        __bindgen_anon_2: spi_transaction_t__bindgen_ty_2 {
            rx_buffer: read as *mut _,
        },
        length: (transaction_length * 8) as _,
        rxlength: (rx_length * 8) as _,
        ..Default::default()
    };

    esp!(unsafe { spi_device_polling_transmit(handle, &mut transaction as *mut _) })
}

macro_rules! impl_spi {
    ($spi:ident: $device:expr) => {
        crate::impl_peripheral!($spi);

        impl Spi for $spi {
            #[inline(always)]
            fn device() -> spi_host_device_t {
                $device
            }
        }
    };
}

macro_rules! impl_spi_any_pins {
    ($spi:ident) => {
        impl SpiAnyPins for $spi {}
    };
}

impl_spi!(SPI1: spi_host_device_t_SPI1_HOST);
impl_spi!(SPI2: spi_host_device_t_SPI2_HOST);
#[cfg(not(esp32c3))]
impl_spi!(SPI3: spi_host_device_t_SPI3_HOST);

impl_spi_any_pins!(SPI2);
#[cfg(not(esp32c3))]
impl_spi_any_pins!(SPI3);
