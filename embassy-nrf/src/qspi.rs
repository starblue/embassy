//! Quad Serial Peripheral Interface (QSPI) flash driver.

#![macro_use]

use core::future::{poll_fn, Future};
use core::marker::PhantomData;
use core::ptr;
use core::task::Poll;

use embassy_hal_internal::drop::OnDrop;
use embassy_hal_internal::{Peri, PeripheralType};
use embassy_sync::waitqueue::AtomicWaker;
use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash};

use crate::gpio::{self, Pin as GpioPin};
use crate::interrupt::typelevel::Interrupt;
use crate::pac::gpio::vals as gpiovals;
use crate::pac::qspi::vals;
pub use crate::pac::qspi::vals::{
    Addrmode as AddressMode, Ppsize as WritePageSize, Readoc as ReadOpcode, Spimode as SpiMode, Writeoc as WriteOpcode,
};
use crate::{interrupt, pac};

/// Deep power-down config.
pub struct DeepPowerDownConfig {
    /// Time required for entering DPM, in units of 16us
    pub enter_time: u16,
    /// Time required for exiting DPM, in units of 16us
    pub exit_time: u16,
}

/// QSPI bus frequency.
pub enum Frequency {
    /// 32 Mhz
    M32 = 0,
    /// 16 Mhz
    M16 = 1,
    /// 10.7 Mhz
    M10_7 = 2,
    /// 8 Mhz
    M8 = 3,
    /// 6.4 Mhz
    M6_4 = 4,
    /// 5.3 Mhz
    M5_3 = 5,
    /// 4.6 Mhz
    M4_6 = 6,
    /// 4 Mhz
    M4 = 7,
    /// 3.6 Mhz
    M3_6 = 8,
    /// 3.2 Mhz
    M3_2 = 9,
    /// 2.9 Mhz
    M2_9 = 10,
    /// 2.7 Mhz
    M2_7 = 11,
    /// 2.5 Mhz
    M2_5 = 12,
    /// 2.3 Mhz
    M2_3 = 13,
    /// 2.1 Mhz
    M2_1 = 14,
    /// 2 Mhz
    M2 = 15,
}

/// QSPI config.
#[non_exhaustive]
pub struct Config {
    /// XIP offset.
    pub xip_offset: u32,
    /// Opcode used for read operations.
    pub read_opcode: ReadOpcode,
    /// Opcode used for write operations.
    pub write_opcode: WriteOpcode,
    /// Page size for write operations.
    pub write_page_size: WritePageSize,
    /// Configuration for deep power down. If None, deep power down is disabled.
    pub deep_power_down: Option<DeepPowerDownConfig>,
    /// QSPI bus frequency.
    pub frequency: Frequency,
    /// Value is specified in number of 16 MHz periods (62.5 ns)
    pub sck_delay: u8,
    /// Value is specified in number of 64 MHz periods (15.625 ns), valid values between 0 and 7 (inclusive)
    pub rx_delay: u8,
    /// Whether data is captured on the clock rising edge and data is output on a falling edge (MODE0) or vice-versa (MODE3)
    pub spi_mode: SpiMode,
    /// Addressing mode (24-bit or 32-bit)
    pub address_mode: AddressMode,
    /// Flash memory capacity in bytes. This is the value reported by the `embedded-storage` traits.
    pub capacity: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            read_opcode: ReadOpcode::READ4IO,
            write_opcode: WriteOpcode::PP4IO,
            xip_offset: 0,
            write_page_size: WritePageSize::_256BYTES,
            deep_power_down: None,
            frequency: Frequency::M8,
            sck_delay: 80,
            rx_delay: 2,
            spi_mode: SpiMode::MODE0,
            address_mode: AddressMode::_24BIT,
            capacity: 0,
        }
    }
}

/// Error
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum Error {
    /// Operation address was out of bounds.
    OutOfBounds,
    // TODO add "not in data memory" error and check for it
}

/// Interrupt handler.
pub struct InterruptHandler<T: Instance> {
    _phantom: PhantomData<T>,
}

impl<T: Instance> interrupt::typelevel::Handler<T::Interrupt> for InterruptHandler<T> {
    unsafe fn on_interrupt() {
        let r = T::regs();
        let s = T::state();

        if r.events_ready().read() != 0 {
            s.waker.wake();
            r.intenclr().write(|w| w.set_ready(true));
        }
    }
}

/// QSPI flash driver.
pub struct Qspi<'d, T: Instance> {
    _peri: Peri<'d, T>,
    dpm_enabled: bool,
    capacity: u32,
}

impl<'d, T: Instance> Qspi<'d, T> {
    /// Create a new QSPI driver.
    pub fn new(
        qspi: Peri<'d, T>,
        _irq: impl interrupt::typelevel::Binding<T::Interrupt, InterruptHandler<T>> + 'd,
        sck: Peri<'d, impl GpioPin>,
        csn: Peri<'d, impl GpioPin>,
        io0: Peri<'d, impl GpioPin>,
        io1: Peri<'d, impl GpioPin>,
        io2: Peri<'d, impl GpioPin>,
        io3: Peri<'d, impl GpioPin>,
        config: Config,
    ) -> Self {
        let r = T::regs();

        macro_rules! config_pin {
            ($pin:ident) => {
                $pin.set_high();
                $pin.conf().write(|w| {
                    w.set_dir(gpiovals::Dir::OUTPUT);
                    w.set_drive(gpiovals::Drive::H0H1);
                    #[cfg(all(feature = "_nrf5340", feature = "_s"))]
                    w.set_mcusel(gpiovals::Mcusel::PERIPHERAL);
                });
                r.psel().$pin().write_value($pin.psel_bits());
            };
        }

        config_pin!(sck);
        config_pin!(csn);
        config_pin!(io0);
        config_pin!(io1);
        config_pin!(io2);
        config_pin!(io3);

        r.ifconfig0().write(|w| {
            w.set_addrmode(config.address_mode);
            w.set_dpmenable(config.deep_power_down.is_some());
            w.set_ppsize(config.write_page_size);
            w.set_readoc(config.read_opcode);
            w.set_writeoc(config.write_opcode);
        });

        if let Some(dpd) = &config.deep_power_down {
            r.dpmdur().write(|w| {
                w.set_enter(dpd.enter_time);
                w.set_exit(dpd.exit_time);
            })
        }

        r.ifconfig1().write(|w| {
            w.set_sckdelay(config.sck_delay);
            w.set_dpmen(false);
            w.set_spimode(config.spi_mode);
            w.set_sckfreq(config.frequency as u8);
        });

        r.iftiming().write(|w| {
            w.set_rxdelay(config.rx_delay & 0b111);
        });

        r.xipoffset().write_value(config.xip_offset);

        T::Interrupt::unpend();
        unsafe { T::Interrupt::enable() };

        // Enable it
        r.enable().write(|w| w.set_enable(true));

        let res = Self {
            _peri: qspi,
            dpm_enabled: config.deep_power_down.is_some(),
            capacity: config.capacity,
        };

        r.events_ready().write_value(0);
        r.intenset().write(|w| w.set_ready(true));

        r.tasks_activate().write_value(1);

        Self::blocking_wait_ready();

        res
    }

    /// Do a custom QSPI instruction.
    pub async fn custom_instruction(&mut self, opcode: u8, req: &[u8], resp: &mut [u8]) -> Result<(), Error> {
        let ondrop = OnDrop::new(Self::blocking_wait_ready);

        let len = core::cmp::max(req.len(), resp.len()) as u8;
        self.custom_instruction_start(opcode, req, len)?;

        self.wait_ready().await;

        self.custom_instruction_finish(resp)?;

        ondrop.defuse();

        Ok(())
    }

    /// Do a custom QSPI instruction, blocking version.
    pub fn blocking_custom_instruction(&mut self, opcode: u8, req: &[u8], resp: &mut [u8]) -> Result<(), Error> {
        let len = core::cmp::max(req.len(), resp.len()) as u8;
        self.custom_instruction_start(opcode, req, len)?;

        Self::blocking_wait_ready();

        self.custom_instruction_finish(resp)?;

        Ok(())
    }

    fn custom_instruction_start(&mut self, opcode: u8, req: &[u8], len: u8) -> Result<(), Error> {
        assert!(req.len() <= 8);

        let mut dat0: u32 = 0;
        let mut dat1: u32 = 0;

        for i in 0..4 {
            if i < req.len() {
                dat0 |= (req[i] as u32) << (i * 8);
            }
        }
        for i in 0..4 {
            if i + 4 < req.len() {
                dat1 |= (req[i + 4] as u32) << (i * 8);
            }
        }

        let r = T::regs();
        r.cinstrdat0().write(|w| w.0 = dat0);
        r.cinstrdat1().write(|w| w.0 = dat1);

        r.events_ready().write_value(0);
        r.intenset().write(|w| w.set_ready(true));

        r.cinstrconf().write(|w| {
            w.set_opcode(opcode);
            w.set_length(vals::Length::from_bits(len + 1));
            w.set_lio2(true);
            w.set_lio3(true);
            w.set_wipwait(true);
            w.set_wren(true);
            w.set_lfen(false);
            w.set_lfstop(false);
        });
        Ok(())
    }

    fn custom_instruction_finish(&mut self, resp: &mut [u8]) -> Result<(), Error> {
        let r = T::regs();

        let dat0 = r.cinstrdat0().read().0;
        let dat1 = r.cinstrdat1().read().0;
        for i in 0..4 {
            if i < resp.len() {
                resp[i] = (dat0 >> (i * 8)) as u8;
            }
        }
        for i in 0..4 {
            if i + 4 < resp.len() {
                resp[i] = (dat1 >> (i * 8)) as u8;
            }
        }
        Ok(())
    }

    fn wait_ready(&mut self) -> impl Future<Output = ()> {
        poll_fn(move |cx| {
            let r = T::regs();
            let s = T::state();
            s.waker.register(cx.waker());
            if r.events_ready().read() != 0 {
                return Poll::Ready(());
            }
            Poll::Pending
        })
    }

    fn blocking_wait_ready() {
        loop {
            let r = T::regs();
            if r.events_ready().read() != 0 {
                break;
            }
        }
    }

    fn start_read(&mut self, address: u32, data: &mut [u8]) -> Result<(), Error> {
        // TODO: Return these as errors instead.
        assert_eq!(data.as_ptr() as u32 % 4, 0);
        assert_eq!(data.len() as u32 % 4, 0);
        assert_eq!(address % 4, 0);

        let r = T::regs();

        r.read().src().write_value(address);
        r.read().dst().write_value(data.as_ptr() as u32);
        r.read().cnt().write(|w| w.set_cnt(data.len() as u32));

        r.events_ready().write_value(0);
        r.intenset().write(|w| w.set_ready(true));
        r.tasks_readstart().write_value(1);

        Ok(())
    }

    fn start_write(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        // TODO: Return these as errors instead.
        assert_eq!(data.as_ptr() as u32 % 4, 0);
        assert_eq!(data.len() as u32 % 4, 0);
        assert_eq!(address % 4, 0);

        let r = T::regs();
        r.write().src().write_value(data.as_ptr() as u32);
        r.write().dst().write_value(address);
        r.write().cnt().write(|w| w.set_cnt(data.len() as u32));

        r.events_ready().write_value(0);
        r.intenset().write(|w| w.set_ready(true));
        r.tasks_writestart().write_value(1);

        Ok(())
    }

    fn start_erase(&mut self, address: u32) -> Result<(), Error> {
        // TODO: Return these as errors instead.
        assert_eq!(address % 4096, 0);

        let r = T::regs();
        r.erase().ptr().write_value(address);
        r.erase().len().write(|w| w.set_len(vals::Len::_4KB));

        r.events_ready().write_value(0);
        r.intenset().write(|w| w.set_ready(true));
        r.tasks_erasestart().write_value(1);

        Ok(())
    }

    /// Raw QSPI read.
    ///
    /// The difference with `read` is that this does not do bounds checks
    /// against the flash capacity. It is intended for use when QSPI is used as
    /// a raw bus, not with flash memory.
    pub async fn read_raw(&mut self, address: u32, data: &mut [u8]) -> Result<(), Error> {
        // Avoid blocking_wait_ready() blocking forever on zero-length buffers.
        if data.is_empty() {
            return Ok(());
        }

        let ondrop = OnDrop::new(Self::blocking_wait_ready);

        self.start_read(address, data)?;
        self.wait_ready().await;

        ondrop.defuse();

        Ok(())
    }

    /// Raw QSPI write.
    ///
    /// The difference with `write` is that this does not do bounds checks
    /// against the flash capacity. It is intended for use when QSPI is used as
    /// a raw bus, not with flash memory.
    pub async fn write_raw(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        // Avoid blocking_wait_ready() blocking forever on zero-length buffers.
        if data.is_empty() {
            return Ok(());
        }

        let ondrop = OnDrop::new(Self::blocking_wait_ready);

        self.start_write(address, data)?;
        self.wait_ready().await;

        ondrop.defuse();

        Ok(())
    }

    /// Raw QSPI read, blocking version.
    ///
    /// The difference with `blocking_read` is that this does not do bounds checks
    /// against the flash capacity. It is intended for use when QSPI is used as
    /// a raw bus, not with flash memory.
    pub fn blocking_read_raw(&mut self, address: u32, data: &mut [u8]) -> Result<(), Error> {
        // Avoid blocking_wait_ready() blocking forever on zero-length buffers.
        if data.is_empty() {
            return Ok(());
        }

        self.start_read(address, data)?;
        Self::blocking_wait_ready();
        Ok(())
    }

    /// Raw QSPI write, blocking version.
    ///
    /// The difference with `blocking_write` is that this does not do bounds checks
    /// against the flash capacity. It is intended for use when QSPI is used as
    /// a raw bus, not with flash memory.
    pub fn blocking_write_raw(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        // Avoid blocking_wait_ready() blocking forever on zero-length buffers.
        if data.is_empty() {
            return Ok(());
        }

        self.start_write(address, data)?;
        Self::blocking_wait_ready();
        Ok(())
    }

    /// Read data from the flash memory.
    pub async fn read(&mut self, address: u32, data: &mut [u8]) -> Result<(), Error> {
        self.bounds_check(address, data.len())?;
        self.read_raw(address, data).await
    }

    /// Write data to the flash memory.
    pub async fn write(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        self.bounds_check(address, data.len())?;
        self.write_raw(address, data).await
    }

    /// Erase a sector on the flash memory.
    pub async fn erase(&mut self, address: u32) -> Result<(), Error> {
        if address >= self.capacity {
            return Err(Error::OutOfBounds);
        }

        let ondrop = OnDrop::new(Self::blocking_wait_ready);

        self.start_erase(address)?;
        self.wait_ready().await;

        ondrop.defuse();

        Ok(())
    }

    /// Read data from the flash memory, blocking version.
    pub fn blocking_read(&mut self, address: u32, data: &mut [u8]) -> Result<(), Error> {
        self.bounds_check(address, data.len())?;
        self.blocking_read_raw(address, data)
    }

    /// Write data to the flash memory, blocking version.
    pub fn blocking_write(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        self.bounds_check(address, data.len())?;
        self.blocking_write_raw(address, data)
    }

    /// Erase a sector on the flash memory, blocking version.
    pub fn blocking_erase(&mut self, address: u32) -> Result<(), Error> {
        if address >= self.capacity {
            return Err(Error::OutOfBounds);
        }

        self.start_erase(address)?;
        Self::blocking_wait_ready();
        Ok(())
    }

    fn bounds_check(&self, address: u32, len: usize) -> Result<(), Error> {
        let len_u32: u32 = len.try_into().map_err(|_| Error::OutOfBounds)?;
        let end_address = address.checked_add(len_u32).ok_or(Error::OutOfBounds)?;
        if end_address > self.capacity {
            return Err(Error::OutOfBounds);
        }
        Ok(())
    }
}

impl<'d, T: Instance> Drop for Qspi<'d, T> {
    fn drop(&mut self) {
        let r = T::regs();

        if self.dpm_enabled {
            trace!("qspi: doing deep powerdown...");

            r.ifconfig1().modify(|w| w.set_dpmen(true));

            // Wait for DPM enter.
            // Unfortunately we must spin. There's no way to do this interrupt-driven.
            // The READY event does NOT fire on DPM enter (but it does fire on DPM exit :shrug:)
            while !r.status().read().dpm() {}

            // Wait MORE for DPM enter.
            // I have absolutely no idea why, but the wait above is not enough :'(
            // Tested with mx25r64 in nrf52840-dk, and with mx25r16 in custom board
            cortex_m::asm::delay(4096);
        }

        // it seems events_ready is not generated in response to deactivate. nrfx doesn't wait for it.
        r.tasks_deactivate().write_value(1);

        // Workaround https://docs.nordicsemi.com/bundle/errata_nRF52840_Rev3/page/ERR/nRF52840/Rev3/latest/anomaly_840_122.html
        // Note that the doc has 2 register writes, but the first one is really the write to tasks_deactivate,
        // so we only do the second one here.
        unsafe { ptr::write_volatile(0x40029054 as *mut u32, 1) }

        r.enable().write(|w| w.set_enable(false));

        // Note: we do NOT deconfigure CSN here. If DPM is in use and we disconnect CSN,
        // leaving it floating, the flash chip might read it as zero which would cause it to
        // spuriously exit DPM.
        gpio::deconfigure_pin(r.psel().sck().read());
        gpio::deconfigure_pin(r.psel().io0().read());
        gpio::deconfigure_pin(r.psel().io1().read());
        gpio::deconfigure_pin(r.psel().io2().read());
        gpio::deconfigure_pin(r.psel().io3().read());

        trace!("qspi: dropped");
    }
}

impl<'d, T: Instance> ErrorType for Qspi<'d, T> {
    type Error = Error;
}

impl NorFlashError for Error {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}

impl<'d, T: Instance> ReadNorFlash for Qspi<'d, T> {
    const READ_SIZE: usize = 4;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.blocking_read(offset, bytes)?;
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.capacity as usize
    }
}

impl<'d, T: Instance> NorFlash for Qspi<'d, T> {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        for address in (from..to).step_by(<Self as NorFlash>::ERASE_SIZE) {
            self.blocking_erase(address)?;
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.blocking_write(offset, bytes)?;
        Ok(())
    }
}

#[cfg(feature = "qspi-multiwrite-flash")]
impl<'d, T: Instance> embedded_storage::nor_flash::MultiwriteNorFlash for Qspi<'d, T> {}

mod _eh1 {
    use embedded_storage_async::nor_flash::{NorFlash as AsyncNorFlash, ReadNorFlash as AsyncReadNorFlash};

    use super::*;

    impl<'d, T: Instance> AsyncNorFlash for Qspi<'d, T> {
        const WRITE_SIZE: usize = <Self as NorFlash>::WRITE_SIZE;
        const ERASE_SIZE: usize = <Self as NorFlash>::ERASE_SIZE;

        async fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
            self.write(offset, data).await
        }

        async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            for address in (from..to).step_by(<Self as AsyncNorFlash>::ERASE_SIZE) {
                self.erase(address).await?
            }
            Ok(())
        }
    }

    impl<'d, T: Instance> AsyncReadNorFlash for Qspi<'d, T> {
        const READ_SIZE: usize = 4;
        async fn read(&mut self, address: u32, data: &mut [u8]) -> Result<(), Self::Error> {
            self.read(address, data).await
        }

        fn capacity(&self) -> usize {
            self.capacity as usize
        }
    }

    #[cfg(feature = "qspi-multiwrite-flash")]
    impl<'d, T: Instance> embedded_storage_async::nor_flash::MultiwriteNorFlash for Qspi<'d, T> {}
}

/// Peripheral static state
pub(crate) struct State {
    waker: AtomicWaker,
}

impl State {
    pub(crate) const fn new() -> Self {
        Self {
            waker: AtomicWaker::new(),
        }
    }
}

pub(crate) trait SealedInstance {
    fn regs() -> pac::qspi::Qspi;
    fn state() -> &'static State;
}

/// QSPI peripheral instance.
#[allow(private_bounds)]
pub trait Instance: SealedInstance + PeripheralType + 'static + Send {
    /// Interrupt for this peripheral.
    type Interrupt: interrupt::typelevel::Interrupt;
}

macro_rules! impl_qspi {
    ($type:ident, $pac_type:ident, $irq:ident) => {
        impl crate::qspi::SealedInstance for peripherals::$type {
            fn regs() -> pac::qspi::Qspi {
                pac::$pac_type
            }
            fn state() -> &'static crate::qspi::State {
                static STATE: crate::qspi::State = crate::qspi::State::new();
                &STATE
            }
        }
        impl crate::qspi::Instance for peripherals::$type {
            type Interrupt = crate::interrupt::typelevel::$irq;
        }
    };
}
