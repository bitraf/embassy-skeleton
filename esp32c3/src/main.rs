#![no_std]
#![no_main]
use core::net::Ipv4Addr;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embassy_net::{Runner, StackResources, tcp::TcpSocket};
use esp_backtrace as _;
use esp_hal::{
	rng::Rng,
	gpio::{
		self, Output, OutputConfig
	},
	i2c::master::{
		Config as I2cConfig, I2c
	},
	time::Rate,
	timer::timg::TimerGroup,
	clock::CpuClock,
	interrupt::software::SoftwareInterruptControl
};
use esp_radio::{
    Controller,
    wifi::{ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState},
};
use esp_println::println;
use ssd1306::{
	command::CommandAsync,
	mode::TerminalDisplaySizeAsync,
	prelude::*,
	size::DisplaySizeAsync,
	I2CDisplayInterface, Ssd1306Async
};
use display_interface::{DisplayError, AsyncWriteOnlyDataCommand};

esp_bootloader_esp_idf::esp_app_desc!();

// Put clear text network SSID and PSK in separate files in the src folder
// The content of these files are included at compile time
// Make sure you don't have any trailing linefeeds or other extra characters!
const SSID: &str = include_str!("SSID.txt");
const PSK: &str = include_str!("PSK.txt");
const DUR: u64 = 500; // Blink interval

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

// Creating a configuration for the tiny display
struct DisplaySize40x20;
impl DisplaySizeAsync for DisplaySize40x20 {
    const WIDTH: u8 = 90;
    const HEIGHT: u8 = 20;
    const OFFSETX: u8 = 27;
    type Buffer = [u8; Self::WIDTH as usize *
        Self::HEIGHT as usize / 8];

    async fn configure(&self, iface: &mut impl AsyncWriteOnlyDataCommand) -> Result<(), DisplayError> {
        CommandAsync::ComPinConfig(false, false).send(iface).await
    }
}
impl TerminalDisplaySizeAsync for DisplaySize40x20 {
    const CHAR_NUM: u8 = 3 * 11; // 2 lines of 9 characters will be completely visible
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    // Initialize timer, led, i2c
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let timg0 = TimerGroup::new(p.TIMG0);
    let sw_int = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(
        timg0.timer0,
        sw_int.software_interrupt0,
    );
    let led = Output::new(p.GPIO8, gpio::Level::Low, OutputConfig::default());
    let i2c = I2c::new(p.I2C0, I2cConfig::default().with_frequency(Rate::from_khz(400))).unwrap()
        .with_scl(p.GPIO6)
        .with_sda(p.GPIO5)
        .into_async();

    // Initialize display
    let mut display = Ssd1306Async::new(I2CDisplayInterface::new(i2c), DisplaySize40x20, DisplayRotation::Rotate0)
        .into_terminal_mode();
    let _ = display.init().await.unwrap();
    let _ = display.clear().await.unwrap();

    Timer::after(Duration::from_secs(1)).await;
    spawner.spawn(blink(led)).ok();

    // Send text to terminal display
    let _ = display.write_str("Hello\nWorld!").await;

    // Heap allocation for esp_radio
    esp_alloc::heap_allocator!(size: 64 * 1024);

    // Configure the radio
    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());
    let (controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, p.WIFI, Default::default()).unwrap();

    let wifi_interface = interfaces.sta;
    let config = embassy_net::Config::dhcpv4(Default::default());
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller, SSID, PSK)).ok();
    spawner.spawn(net_task(runner)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_millis(1_000)).await;
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));
        let remote_endpoint = (Ipv4Addr::new(142, 250, 185, 115), 80);
        println!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected!");
        let mut buf = [0; 1024];
        loop {
            use embedded_io_async::Write;
            let r = socket
                .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
                .await;
            if let Err(e) = r {
                println!("write error: {:?}", e);
                break;
            }
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    println!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    println!("read error: {:?}", e);
                    break;
                }
            };
            println!("{}", core::str::from_utf8(&buf[..n]).unwrap());
        }
        Timer::after(Duration::from_secs(10)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>, ssid: &'static str, psk: &'static str) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(ssid.into())
                    .with_password(psk.into())
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            println!("Scan");
            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                println!("{:?}", ap);
            }
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn blink(mut led: Output<'static>) {
    loop {
	    led.toggle();
		Timer::after_millis(DUR).await;
    }
}
