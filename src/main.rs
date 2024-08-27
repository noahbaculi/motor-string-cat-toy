#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcCalLine, AdcConfig, AdcPin, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::GpioPin;
use esp_hal::ledc::timer::TimerIFace;
use esp_hal::peripherals::ADC1;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::timer::{ErasedTimer, OneShotTimer};
use esp_hal::{
    clock::ClockControl,
    gpio::{Io, OutputPin},
    ledc::{
        channel,
        timer::{self, TimerSpeed},
        LSGlobalClkSource, Ledc, LowSpeed,
    },
    peripheral::Peripheral,
    peripherals::Peripherals,
    prelude::*,
    system::SystemControl,
};
use esp_println::dbg;
use log::{debug, info};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use static_cell::StaticCell;

const NUM_ADC_SAMPLES: usize = 100; // Number of ADC samples to average
const MAX_ACTIVE_SEC: u16 = 10 * 60; // Number of seconds the device will be active before going to deep sleep
const MIN_MOTOR_DUTY_PERCENT: u8 = 20;
const MAX_MOTOR_DUTY_PERCENT: u8 = 100;
const MIN_MOVEMENT_DURATION: u16 = 200; // ms
const MAX_MOVEMENT_DURATION: u16 = 2_000; // ms
const POTENTIOMETER_READ_INTERVAL: u8 = 200; // ms

const MIN_ADC_VOLTAGE: u16 = 0; // mV
const MAX_ADC_VOLTAGE: u16 = 3000; // mV

static CURRENT_MAX_MOTOR_PERCENT: AtomicU8 = AtomicU8::new(MIN_MOTOR_DUTY_PERCENT);
static CURRENT_MAX_MOVEMENT_DURATION: AtomicU16 = AtomicU16::new(MIN_MOVEMENT_DURATION);
static DRASTIC_PARAMETER_CHANGE: AtomicBool = AtomicBool::new(false);

type RtcMutex = Mutex<CriticalSectionRawMutex, Rtc<'static>>;

type Adc1Calibration = AdcCalLine<ADC1>;
type Adc1Mutex = Mutex<CriticalSectionRawMutex, Adc<'static, ADC1>>;
type AdcPin0MutexForSpeed =
    Mutex<CriticalSectionRawMutex, AdcPin<GpioPin<0>, ADC1, Adc1Calibration>>;
type AdcPin1MutexForDuration =
    Mutex<CriticalSectionRawMutex, AdcPin<GpioPin<1>, ADC1, Adc1Calibration>>;

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[main]
async fn main(spawner: Spawner) {
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock80MHz).freeze();
    esp_println::logger::init_logger(log::LevelFilter::Debug);
    let timer_grp = TimerGroup::new(peripherals.TIMG0, &clocks, None);
    esp_hal_embassy::init(
        &clocks,
        mk_static!(
            [OneShotTimer<ErasedTimer>; 1],
            [OneShotTimer::new(timer_grp.timer0.into())]
        ),
    );
    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    static RTC: StaticCell<RtcMutex> = StaticCell::new();
    let rtc = RTC.init(Mutex::new(Rtc::new(peripherals.LPWR, None)));

    let motor_pwm_pin_forward = io.pins.gpio18;
    let motor_pwm_pin_reverse = io.pins.gpio19;

    // Instantiate PWM infra
    let mut ledc_pwm_controller = Ledc::new(peripherals.LEDC, &clocks);
    ledc_pwm_controller.set_global_slow_clock(LSGlobalClkSource::APBClk);
    let mut pwm_timer = ledc_pwm_controller.get_timer::<LowSpeed>(timer::Number::Timer0);
    pwm_timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty14Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: 2.kHz(),
        })
        .unwrap();

    let mut motor = Motor::new(
        &ledc_pwm_controller,
        &pwm_timer,
        channel::Number::Channel0,
        channel::Number::Channel1,
        motor_pwm_pin_forward,
        motor_pwm_pin_reverse,
    );

    // Instantiate ADC and mutexes
    let mut adc1_config = AdcConfig::new();
    static SPEED_STATIC_CELL: StaticCell<AdcPin0MutexForSpeed> = StaticCell::new();
    let speed_pot_pin = SPEED_STATIC_CELL.init(Mutex::new(
        adc1_config
            .enable_pin_with_cal::<_, Adc1Calibration>(io.pins.gpio0, Attenuation::Attenuation11dB),
    ));
    static DURATION_STATIC_CELL: StaticCell<AdcPin1MutexForDuration> = StaticCell::new();
    let duration_pot_pin = DURATION_STATIC_CELL.init(Mutex::new(
        adc1_config
            .enable_pin_with_cal::<_, Adc1Calibration>(io.pins.gpio1, Attenuation::Attenuation11dB),
    ));
    let adc1 = Adc::new(peripherals.ADC1, adc1_config);
    static ADC1_MUTEX: StaticCell<Adc1Mutex> = StaticCell::new();
    let adc1 = ADC1_MUTEX.init(Mutex::new(adc1));

    spawner.must_spawn(deep_sleep_countdown(rtc));
    spawner.must_spawn(monitor_speed_pot(adc1, speed_pot_pin));
    spawner.must_spawn(monitor_duration_pot(adc1, duration_pot_pin));

    let mut small_rng = SmallRng::seed_from_u64(1); // seed irrelevant for random number generation
    let mut ticker = Ticker::every(Duration::from_millis(200));
    for direction in [MotorDirection::Forward, MotorDirection::Reverse]
        .iter()
        .cycle()
    {
        let start_time = Instant::now();
        let max_motor_percent = CURRENT_MAX_MOTOR_PERCENT.load(Ordering::Relaxed);
        let duty_percent = small_rng.gen_range(MIN_MOTOR_DUTY_PERCENT..=max_motor_percent);
        motor.start_movement(direction, duty_percent);
        DRASTIC_PARAMETER_CHANGE.store(false, Ordering::Relaxed);

        let movement_duration = Duration::from_millis(2 * 1_000);

        while Instant::now().duration_since(start_time) >= movement_duration {
            ticker.next().await;
            if DRASTIC_PARAMETER_CHANGE.load(Ordering::Relaxed) {
                break; // Break the loop if there is a drastic parameter change
            }
        }
    }
}

#[embassy_executor::task]
async fn monitor_speed_pot(
    adc1_mutex: &'static Adc1Mutex,
    speed_pot_pin_mutex: &'static AdcPin0MutexForSpeed,
) {
    let mut ticker = Ticker::every(Duration::from_millis(POTENTIOMETER_READ_INTERVAL.into()));
    let mut prev_max_duty_percent = MIN_MOTOR_DUTY_PERCENT;
    loop {
        debug!("Checking speed pot pin (#0)");
        {
            let mut adc1 = adc1_mutex.lock().await;
            let mut speed_pot_pin = speed_pot_pin_mutex.lock().await;
            let avg_pin_value: u16 = ((0..NUM_ADC_SAMPLES)
                .map(|_| nb::block!(adc1.read_oneshot(&mut speed_pot_pin)).unwrap() as u32)
                .sum::<u32>()
                / NUM_ADC_SAMPLES as u32)
                .try_into()
                .expect("Average of ADC readings is too large to fit into u16");
            dbg!("Average speed pot pin value: {}", avg_pin_value);
            let max_duty_percent: u8 = map_range(
                avg_pin_value,
                MIN_ADC_VOLTAGE,
                MAX_ADC_VOLTAGE,
                MIN_MOTOR_DUTY_PERCENT.into(),
                MAX_MOTOR_DUTY_PERCENT.into(),
            )
            .try_into()
            .expect("Max duty percent is too large to fit into u8");
            dbg!("Max duty percent: {}", max_duty_percent);
            CURRENT_MAX_MOTOR_PERCENT.store(max_duty_percent, Ordering::Relaxed);

            if prev_max_duty_percent.abs_diff(max_duty_percent) > 10 {
                DRASTIC_PARAMETER_CHANGE.store(true, Ordering::Relaxed);
            }

            prev_max_duty_percent = max_duty_percent;
        }
        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn monitor_duration_pot(
    adc1_mutex: &'static Adc1Mutex,
    duration_pot_pin_mutex: &'static AdcPin1MutexForDuration,
) {
    let mut ticker = Ticker::every(Duration::from_millis(POTENTIOMETER_READ_INTERVAL.into()));
    let mut prev_max_duration = MIN_MOVEMENT_DURATION;
    loop {
        debug!("Checking duration pot pin (#1)");
        {
            let mut adc1 = adc1_mutex.lock().await;
            let mut duration_pot_pin = duration_pot_pin_mutex.lock().await;
            let avg_pin_value: u16 = ((0..NUM_ADC_SAMPLES)
                .map(|_| nb::block!(adc1.read_oneshot(&mut duration_pot_pin)).unwrap() as u32)
                .sum::<u32>()
                / NUM_ADC_SAMPLES as u32)
                .try_into()
                .expect("Average of ADC readings is too large to fit into u16");
            dbg!("Average duration pot pin value: {}", avg_pin_value);
            let max_duration = map_range(
                avg_pin_value,
                MIN_ADC_VOLTAGE,
                MAX_ADC_VOLTAGE,
                MIN_MOVEMENT_DURATION,
                MAX_MOVEMENT_DURATION,
            );
            dbg!("Max duration: {}", max_duration);
            CURRENT_MAX_MOVEMENT_DURATION.store(max_duration, Ordering::Relaxed);

            if prev_max_duration.abs_diff(max_duration) > 10 {
                DRASTIC_PARAMETER_CHANGE.store(true, Ordering::Relaxed);
            }

            prev_max_duration = max_duration;
        }
        ticker.next().await;
    }
}

fn map_range<T>(in_value: T, in_min: T, in_max: T, out_min: T, out_max: T) -> T
where
    T: Copy
        + core::ops::Mul<Output = T>
        + core::ops::Add<Output = T>
        + core::ops::Div<Output = T>
        + core::ops::Sub<Output = T>,
{
    ((in_value - in_min) * (out_max - out_min) / (in_max - in_min)) + out_min
}

#[embassy_executor::task]
async fn deep_sleep_countdown(rtc: &'static RtcMutex) {
    Timer::after(Duration::from_secs(MAX_ACTIVE_SEC.into())).await;
    info!("{} seconds passed, going to deep sleep", MAX_ACTIVE_SEC);
    rtc.lock().await.sleep_deep(&[]);
}

enum MotorDirection {
    Forward,
    Reverse,
}

struct Motor<'a, S, O1, O2>
where
    S: TimerSpeed,
    O1: OutputPin,
    O2: OutputPin,
{
    pwm_channel_forward: channel::Channel<'a, S, O1>,
    pwm_channel_reverse: channel::Channel<'a, S, O2>,
}

impl<'a, S, O1, O2> Motor<'a, S, O1, O2>
where
    S: TimerSpeed,
    O1: OutputPin,
    O2: OutputPin,
{
    fn new(
        ledc_pwm_controller: &'a Ledc,
        pwm_timer: &'a dyn TimerIFace<S>,
        pwm_channel_forward_number: channel::Number,
        pwm_channel_reverse_number: channel::Number,
        pwm_pin_forward: impl Peripheral<P = O1> + 'a,
        pwm_pin_reverse: impl Peripheral<P = O2> + 'a,
    ) -> Self {
        // Instantiate PWM channels
        let mut pwm_channel_forward =
            ledc_pwm_controller.get_channel(pwm_channel_forward_number, pwm_pin_forward);
        pwm_channel_forward
            .configure(channel::config::Config {
                timer: pwm_timer,
                duty_pct: 0,
                pin_config: channel::config::PinConfig::PushPull,
            })
            .unwrap();

        let mut pwm_channel_reverse =
            ledc_pwm_controller.get_channel(pwm_channel_reverse_number, pwm_pin_reverse);
        pwm_channel_reverse
            .configure(channel::config::Config {
                timer: pwm_timer,
                duty_pct: 0,
                pin_config: channel::config::PinConfig::PushPull,
            })
            .unwrap();

        Self {
            pwm_channel_forward,
            pwm_channel_reverse,
        }
    }

    fn start_movement(&mut self, direction: &MotorDirection, duty_percent: u8) {
        match direction {
            MotorDirection::Forward => {
                self.pwm_channel_forward.set_duty(duty_percent).unwrap();
                self.pwm_channel_reverse.set_duty(0).unwrap();
            }
            MotorDirection::Reverse => {
                self.pwm_channel_forward.set_duty(0).unwrap();
                self.pwm_channel_reverse.set_duty(duty_percent).unwrap();
            }
        }
    }

    fn stop(&mut self) {
        self.pwm_channel_forward.set_duty(0).unwrap();
        self.pwm_channel_reverse.set_duty(0).unwrap();
    }
}
