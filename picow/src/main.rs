#![no_std]
#![no_main]
use {
    core::{
    	net::Ipv4Addr, str, time::Duration
    },
    cyw43::{
    	Control, JoinOptions
    },
    cyw43_pio::{
    	DEFAULT_CLOCK_DIVIDER, PioSpi
    },
    defmt::{
    	info, error,
    },
    defmt_rtt as _,
    embassy_executor::{
    	Executor, Spawner
    },
    embassy_net::{
        Config, DhcpConfig, StackResources, dns::DnsSocket,
        tcp::client::{
        	TcpClient, TcpClientState
        },
    },
    embassy_rp::{
        bind_interrupts,
        clocks::RoscRng,
        gpio::{
        	Level, Output
        },
        peripherals::{
        	DMA_CH0, PIO0, *
        },
        pio::{
        	InterruptHandler, Pio
        },
    },
    embassy_sync::{
    	blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex
    },
    embassy_time::Timer,
    panic_probe as _,
    rand::RngCore,
    reqwless::{
    	client::HttpClient,
    	// client::{HttpClient, TlsConfig, TlsVerify},
     	request::Method,
    },
    static_cell::StaticCell,
};
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static STATE: StaticCell<cyw43::State> = StaticCell::new();
static CONTROL: StaticCell<Mutex<CriticalSectionRawMutex, cyw43::Control<'_>>> = StaticCell::new();
bind_interrupts!(struct Irqs { PIO0_IRQ_0 => InterruptHandler<PIO0>; });

// Put cleartext network SSID and PSK in separate files in the src folder
// The content of these files are included at compile time
// Make sure you don't have any trailing linefeeds or other extra characters!
const SSID: &str = include_str!("SSID.txt");
const PSK: &str = include_str!("PSK.txt");
const DUR: u64 = 500; // Blink interval

struct WifiPerfs {
    pwr: PIN_23,
    dio: PIN_24,
    cs: PIN_25,
    clk: PIN_29,
    pio0: PIO0,
    dma: DMA_CH0,
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());
    // let led = Output::new(p.  PIN_17, Level::Low);
    // Dedicate peripherals for net chip use
    let perfs_wifi = WifiPerfs {pwr: p.PIN_23, dio: p.PIN_24, cs: p.PIN_25, clk: p.PIN_29, pio0: p.PIO0, dma: p.DMA_CH0};

    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(move |spawner| spawner.spawn(core0_task(spawner, perfs_wifi)).unwrap());
}

#[embassy_executor::task]
async fn core0_task(spawner: Spawner, perfs: WifiPerfs) {
	info!("core task started");

    // Include the WiFi firmware and Country Locale Matrix (CLM) blobs; initially, the following binaries must be downloaded to flash
    // probe-rs download src/43439A0.bin --binary-format bin --chip RP2040 --base-address 0x10100000
    // probe-rs download src/43439A0_clm.bin --binary-format bin --chip RP2040 --base-address 0x10140000
    let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 231077) };
    let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 984) };
    let state = STATE.init(cyw43::State::new());
    let cs = Output::new(perfs.cs, Level::High);
    let pwr = Output::new(perfs.pwr, Level::Low);
    let mut pio = Pio::new(perfs.pio0, Irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, DEFAULT_CLOCK_DIVIDER, pio.irq0, cs, perfs.dio, perfs.clk, perfs.dma);
    let (net_device, control, runner) = cyw43::new(state, pwr, spi, fw).await;
    let _ = spawner.spawn(cyw43_task(runner));
    let control = CONTROL.init(Mutex::new(control));
    control.lock().await.init(clm).await;
    control.lock().await.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    // Init network stack
    info!("init network stack");
    let mut rng = RoscRng;
    let seed = rng.next_u64();
    let mut dhcp_config = DhcpConfig::default();
    dhcp_config.retry_config.initial_request_timeout = Duration::from_secs(5).into();
    dhcp_config.retry_config.request_retries = 10;
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) =
        embassy_net::new(net_device, Config::dhcpv4(dhcp_config), RESOURCES.init(StackResources::new()), seed);
    let _ = spawner.spawn(net_task(runner));

    // Persistently try and join WiFi network
    while let Err(err) = control.lock().await.join(SSID, JoinOptions::new(PSK.as_bytes())).await {
        error!("join failed with status: {}", err.status);
        Timer::after_secs(5).await;
    }

    // Wait for DHCP
    info!("waiting for IP address...");
    stack.wait_config_up().await;

    // Initiate blink
    spawner.spawn(blink(control)).unwrap();

    let client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &client_state);
    let dns_client = DnsSocket::new(stack);

    let endpoint = (Ipv4Addr::new(142, 250, 185, 115), 80);
    let mut tmp_buf = [0u8; 71];
    let url = format_no_std::show(&mut tmp_buf,
    	format_args!("http://{}.{}.{}.{}:{} GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n",
     	endpoint.0.octets()[0], endpoint.0.octets()[1], endpoint.0.octets()[2], endpoint.0.octets()[3], endpoint.1)
    ).unwrap();

    let mut http_client = HttpClient::new(&tcp_client, &dns_client);

    // Persistently try and make request
    loop {
	    let mut rx_buffer = [0u8; 4096];
	    let mut request = {
	    	loop {
	     		match http_client.request(Method::GET, url).await {
	       			Ok(request) => break request,
	          		Err(err) => error!("Failed to make HTTP request: {:?}", err),
	       		}
	         	Timer::after_secs(5).await;
	     	}
	    };

	    // Persistently try and send request
	    let response = {
	    	loop {
	     		match request.send(&mut rx_buffer).await {
	       			Ok(response) => break response,
	          		Err(err) => error!("Failed to send HTTP request: {:?}", err),
	       		}
	         	Timer::after_secs(5).await;
	     	}
	    };

	    if let Ok(body) = str::from_utf8(response.body().read_to_end().await.unwrap()) {
	    	info!("{}", body);
	    } else {
	    	error!("Failed to read response body");
	    }

		Timer::after_secs(10).await;
    }
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn blink(control: &'static mut Mutex<CriticalSectionRawMutex, Control<'static>>) {
	loop {
		control.lock().await.gpio_set(0, true).await;
		Timer::after_millis(DUR).await;
		control.lock().await.gpio_set(0, false).await;
		Timer::after_millis(DUR).await;
	}
}
