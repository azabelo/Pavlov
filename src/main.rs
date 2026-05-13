use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

mod signals;
use signals::{RuleScope, SignalConfig, SignalCooldowns, SignalRule, SignalRuleKind};

const VIBE_UUID: &str = "00001001-0000-1000-8000-00805f9b34fb";
const BEEP_UUID: &str = "00001002-0000-1000-8000-00805f9b34fb";
const ZAP_UUID: &str = "00001003-0000-1000-8000-00805f9b34fb";
const LEDS_UUID: &str = "00001004-0000-1000-8000-00805f9b34fb";
const TIME_UUID: &str = "00001005-0000-1000-8000-00805f9b34fb";
const ALARM_TIME_UUID: &str = "0000200a-0000-1000-8000-00805f9b34fb";
const ALARM_CONTROL_UUID: &str = "00005001-0000-1000-8000-00805f9b34fb";
const ALARM_WRITE_UUID: &str = "00005002-0000-1000-8000-00805f9b34fb";
const SETUP_UUID: &str = "00007001-0000-1000-8000-00805f9b34fb";
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_ACTION_TIMEOUT: Duration = Duration::from_secs(10);
const BLE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const WAKEUP_ALARM_PROFILE: &str = "Pavlov Wake";
const WAKEUP_ALARM_ID_BASE: u16 = 905;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum Stimulus {
    Vibe,
    Beep,
    Zap,
}

impl Stimulus {
    fn uuid(self) -> Uuid {
        match self {
            Stimulus::Vibe => Uuid::parse_str(VIBE_UUID).expect("valid vibe uuid"),
            Stimulus::Beep => Uuid::parse_str(BEEP_UUID).expect("valid beep uuid"),
            Stimulus::Zap => Uuid::parse_str(ZAP_UUID).expect("valid zap uuid"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Stimulus::Vibe => "vibe",
            Stimulus::Beep => "beep",
            Stimulus::Zap => "zap",
        }
    }
}

impl std::fmt::Display for Stimulus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Stimulus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "vibe" => Ok(Stimulus::Vibe),
            "beep" => Ok(Stimulus::Beep),
            "zap" => Ok(Stimulus::Zap),
            _ => bail!("unknown stimulus: {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WriteMode {
    #[value(name = "response")]
    Response,
    #[value(name = "no-response")]
    NoResponse,
}

impl WriteMode {
    fn as_str(self) -> &'static str {
        match self {
            WriteMode::Response => "response",
            WriteMode::NoResponse => "no-response",
        }
    }

    fn btleplug(self) -> WriteType {
        match self {
            WriteMode::Response => WriteType::WithResponse,
            WriteMode::NoResponse => WriteType::WithoutResponse,
        }
    }
}

impl std::fmt::Display for WriteMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Parser)]
#[command(name = "pavlovd-rs")]
#[command(about = "Low-latency local BLE control for Pavlok 3")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Scan(CommonArgs),
    Once(OnceArgs),
    Serve(ServeArgs),
    Stdin(CommonArgs),
    Monitor(MonitorArgs),
    Rules(RulesArgs),
}

#[derive(Clone, Debug, Parser)]
struct CommonArgs {
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    uuid: Option<String>,
    #[arg(long, default_value_t = 8_000)]
    scan_timeout_ms: u64,
    #[arg(long, default_value_t = 15_000)]
    connect_timeout_ms: u64,
    #[arg(long, default_value_t = WriteMode::Response)]
    mode: WriteMode,
    #[arg(long, default_value_t = 50)]
    intensity: u8,
    #[arg(long, default_value_t = 1)]
    count: u8,
    #[arg(long, default_value_t = 22)]
    on_ms: u8,
    #[arg(long, default_value_t = 22)]
    off_ms: u8,
    #[arg(long, default_value_t = false)]
    allow_zap: bool,
    #[arg(long)]
    log_file: Option<String>,
}

#[derive(Clone, Debug, Parser)]
struct OnceArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, default_value_t = Stimulus::Vibe)]
    stim: Stimulus,
    #[arg(long, default_value_t = 10)]
    samples: usize,
    #[arg(long, default_value_t = 500)]
    interval_ms: u64,
}

#[derive(Clone, Debug, Parser)]
struct ServeArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8765)]
    port: u16,
}

#[derive(Clone, Debug, Parser)]
struct MonitorArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, default_value_t = 1_000)]
    poll_ms: u64,
    #[arg(long, default_value_t = 30)]
    cooldown_secs: u64,
    #[arg(long)]
    rules_file: Option<String>,
    #[arg(long)]
    dnd_command: Option<String>,
    #[arg(long, default_value = "http://127.0.0.1:8765/stim/zap")]
    zap_url: String,
    #[arg(long, default_value_t = false)]
    direct_ble: bool,
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Clone, Debug, Parser)]
struct RulesArgs {
    #[arg(long, global = true)]
    rules_file: Option<String>,
    #[command(subcommand)]
    command: RulesCommand,
}

#[derive(Clone, Debug, Subcommand)]
enum RulesCommand {
    List,
    Path,
    AddApp {
        name: String,
        #[arg(long, default_value_t = RuleScope::Dnd)]
        scope: RuleScope,
    },
    AddSite {
        pattern: String,
        #[arg(long, default_value_t = RuleScope::Always)]
        scope: RuleScope,
    },
    RemoveApp {
        name: String,
        #[arg(long)]
        scope: Option<RuleScope>,
    },
    RemoveSite {
        pattern: String,
        #[arg(long)]
        scope: Option<RuleScope>,
    },
}

#[derive(Clone, Debug)]
struct StimRequest {
    stim: Stimulus,
    intensity: u8,
    count: u8,
    on_ms: u8,
    off_ms: u8,
    mode: WriteMode,
}

#[derive(Clone)]
struct PavlokClient {
    peripheral: Peripheral,
    chars: HashMap<StimulusKey, Characteristic>,
    allow_zap: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum StimulusKey {
    Vibe,
    Beep,
    Zap,
    Leds,
    Time,
    AlarmTime,
    AlarmControl,
    AlarmWrite,
    Setup,
}

impl From<Stimulus> for StimulusKey {
    fn from(value: Stimulus) -> Self {
        match value {
            Stimulus::Vibe => StimulusKey::Vibe,
            Stimulus::Beep => StimulusKey::Beep,
            Stimulus::Zap => StimulusKey::Zap,
        }
    }
}

impl StimulusKey {
    fn as_str(self) -> &'static str {
        match self {
            StimulusKey::Vibe => "c_vibe",
            StimulusKey::Beep => "c_beep",
            StimulusKey::Zap => "c_zap",
            StimulusKey::Leds => "c_leds",
            StimulusKey::Time => "c_time",
            StimulusKey::AlarmTime => "c_atime",
            StimulusKey::AlarmControl => "c_actl",
            StimulusKey::AlarmWrite => "c_awrite",
            StimulusKey::Setup => "c_setup",
        }
    }
}

struct VibeAlarmRequest {
    minutes: u16,
    intensity: u8,
    count: u8,
    name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct WakeupAlarmState {
    profile: String,
    alarms: Vec<WakeupAlarmRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct WakeupAlarmRecord {
    name: String,
    stim: Stimulus,
    scheduled_time: String,
    scheduled_hour: u8,
    scheduled_minute: u8,
    alarm_id: u16,
    intensity: u8,
    count: u8,
}

struct AlarmEntry {
    name: String,
    stim: Stimulus,
    hour: u8,
    minute: u8,
    alarm_id: u16,
    intensity: u8,
    count: u8,
    active: bool,
}

impl AlarmEntry {
    fn record(&self) -> WakeupAlarmRecord {
        WakeupAlarmRecord {
            name: self.name.clone(),
            stim: self.stim,
            scheduled_time: format!("{:02}:{:02}", self.hour, self.minute),
            scheduled_hour: self.hour,
            scheduled_minute: self.minute,
            alarm_id: self.alarm_id,
            intensity: self.intensity,
            count: self.count,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    ok: bool,
    error: String,
    timing_ms: Timing,
}

#[derive(Serialize)]
struct HealthBody {
    ok: bool,
    connected: bool,
    timing_ms: Timing,
}

#[derive(Serialize)]
struct SendBody {
    ok: bool,
    stim: &'static str,
    mode: &'static str,
    payload_hex: String,
    timing_ms: Timing,
}

#[derive(Serialize)]
struct WebtoolBody {
    ok: bool,
    command: &'static str,
    characteristic: String,
    mode: &'static str,
    payload_hex: String,
    timing_ms: Timing,
}

#[derive(Serialize)]
struct AlarmScheduleBody {
    ok: bool,
    name: String,
    scheduled_time: String,
    scheduled_hour: u8,
    scheduled_minute: u8,
    alarm_id: u16,
    payload_hex: String,
    chunks: usize,
    next_alarm: Option<String>,
    timing_ms: Timing,
}

#[derive(Serialize)]
struct AlarmBatchBody {
    ok: bool,
    action: &'static str,
    profile: String,
    alarms: Vec<WakeupAlarmRecord>,
    payload_hex: String,
    chunks: usize,
    next_alarm: Option<String>,
    state_file: String,
    timing_ms: Timing,
}

struct WriteResult {
    payload_hex: String,
    timing_ms: Timing,
}

struct WebtoolCommand {
    name: &'static str,
    writes: Vec<WebtoolWrite>,
    requires_zap: bool,
    mode: WriteMode,
}

struct WebtoolWrite {
    characteristic: StimulusKey,
    payload: Vec<u8>,
}

#[derive(Default, Serialize)]
struct Timing {
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_to_write_issued: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    write_call_wall: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_to_write_ack: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_to_response: Option<f64>,
}

#[derive(Serialize)]
struct ConnectBody {
    connected: bool,
    cold_connect_ms: f64,
}

#[derive(Deserialize)]
struct CloudStimulusBody {
    stimulus: CloudStimulus,
}

#[derive(Deserialize)]
struct CloudStimulus {
    #[serde(rename = "stimulusType")]
    stimulus_type: String,
    #[serde(rename = "stimulusValue")]
    stimulus_value: u16,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(args) => {
            setup_logging(args.log_file.as_deref())?;
            let adapter = default_adapter().await?;
            let devices =
                scan(&adapter, &args, Duration::from_millis(args.scan_timeout_ms)).await?;
            if devices.is_empty() {
                println!("No Pavlok devices found.");
            } else {
                for (id, name, rssi) in devices {
                    println!("{id} {name} rssi={rssi}");
                }
            }
        }
        Command::Once(args) => {
            setup_logging(args.common.log_file.as_deref())?;
            validate_zap_config(&args.common)?;
            ensure_zap_allowed(args.stim, &args.common)?;
            let started = Instant::now();
            let client = connect(&args.common).await?;
            print_json(&ConnectBody {
                connected: true,
                cold_connect_ms: ms_since(started),
            })?;
            let mut samples = Vec::with_capacity(args.samples);
            for _ in 0..args.samples {
                let result = client
                    .send(default_request(&args.common, args.stim), Instant::now())
                    .await?;
                samples.push(sample_latency_ms(&result.timing_ms));
                print_json(&result)?;
                sleep(Duration::from_millis(args.interval_ms)).await;
            }
            println!("sample_summary {}", sample_summary(&samples));
        }
        Command::Serve(args) => {
            setup_logging(args.common.log_file.as_deref())?;
            validate_zap_config(&args.common)?;
            let started = Instant::now();
            let client = connect(&args.common).await?;
            print_json(&ConnectBody {
                connected: true,
                cold_connect_ms: ms_since(started),
            })?;
            serve(args, Arc::new(Mutex::new(client))).await?;
        }
        Command::Stdin(args) => {
            setup_logging(args.log_file.as_deref())?;
            validate_zap_config(&args)?;
            let started = Instant::now();
            let client = connect(&args).await?;
            print_json(&ConnectBody {
                connected: true,
                cold_connect_ms: ms_since(started),
            })?;
            run_stdin(args, client).await?;
        }
        Command::Monitor(args) => {
            setup_logging(args.common.log_file.as_deref())?;
            if !args.dry_run && args.direct_ble {
                validate_zap_config(&args.common)?;
                ensure_zap_allowed(Stimulus::Zap, &args.common)?;
            }
            let config = SignalConfig::load(args.rules_file.as_deref())?;
            print_json(&serde_json::json!({
                "ok": true,
                "monitoring": true,
                "rules_file": SignalConfig::path(args.rules_file.as_deref())?,
                "rules": config.rules.len(),
                "dry_run": args.dry_run,
                "zap_url": args.zap_url,
                "direct_ble": args.direct_ble,
            }))?;
            if args.dry_run || !args.direct_ble {
                run_monitor(args, None, config).await?;
            } else {
                let started = Instant::now();
                let client = connect(&args.common).await?;
                print_json(&ConnectBody {
                    connected: true,
                    cold_connect_ms: ms_since(started),
                })?;
                run_monitor(args, Some(client), config).await?;
            }
        }
        Command::Rules(args) => run_rules(args)?,
    }
    Ok(())
}

async fn default_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("create BLE manager")?;
    let adapters = manager.adapters().await.context("list BLE adapters")?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No Bluetooth adapters found."))
}

async fn scan(
    adapter: &Adapter,
    args: &CommonArgs,
    duration: Duration,
) -> Result<Vec<(String, String, i16)>> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start BLE scan")?;
    sleep(duration).await;
    adapter.stop_scan().await.context("stop BLE scan")?;
    let mut devices = Vec::new();
    for peripheral in adapter.peripherals().await.context("list peripherals")? {
        let Some(props) = peripheral
            .properties()
            .await
            .context("read peripheral properties")?
        else {
            continue;
        };
        let name = props
            .local_name
            .unwrap_or_else(|| peripheral.id().to_string());
        if !is_pavlok_name(&name) && !matches_uuid(&peripheral, args) {
            continue;
        }
        if !matches_target(&peripheral, &name, args) {
            continue;
        }
        devices.push((peripheral.id().to_string(), name, props.rssi.unwrap_or(0)));
    }
    devices.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(devices)
}

async fn connect(args: &CommonArgs) -> Result<PavlokClient> {
    let adapter = default_adapter().await?;
    let timeout_duration = Duration::from_millis(args.connect_timeout_ms);
    timeout(timeout_duration, async {
        adapter
            .start_scan(ScanFilter::default())
            .await
            .context("start BLE scan")?;
        loop {
            for peripheral in adapter.peripherals().await.context("list peripherals")? {
                let Some(props) = peripheral
                    .properties()
                    .await
                    .context("read peripheral properties")?
                else {
                    continue;
                };
                let name = props
                    .local_name
                    .unwrap_or_else(|| peripheral.id().to_string());
                if matches_target(&peripheral, &name, args) {
                    adapter.stop_scan().await.context("stop BLE scan")?;
                    return connect_peripheral(peripheral, args.allow_zap).await;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context(
        "Timed out finding/connecting to Pavlok. Disconnect Chrome/phone and try scan first.",
    )?
}

async fn connect_peripheral(peripheral: Peripheral, allow_zap: bool) -> Result<PavlokClient> {
    if !peripheral
        .is_connected()
        .await
        .context("check connection state")?
    {
        peripheral.connect().await.context("connect Pavlok")?;
    }
    peripheral
        .discover_services()
        .await
        .context("discover services")?;

    let mut chars = HashMap::new();
    for characteristic in peripheral.characteristics() {
        if characteristic.uuid == Stimulus::Vibe.uuid() {
            chars.insert(StimulusKey::Vibe, characteristic);
        } else if characteristic.uuid == Stimulus::Beep.uuid() {
            chars.insert(StimulusKey::Beep, characteristic);
        } else if characteristic.uuid == Stimulus::Zap.uuid() {
            chars.insert(StimulusKey::Zap, characteristic);
        } else if characteristic.uuid == Uuid::parse_str(LEDS_UUID).expect("valid leds uuid") {
            chars.insert(StimulusKey::Leds, characteristic);
        } else if characteristic.uuid == Uuid::parse_str(TIME_UUID).expect("valid time uuid") {
            chars.insert(StimulusKey::Time, characteristic);
        } else if characteristic.uuid
            == Uuid::parse_str(ALARM_TIME_UUID).expect("valid alarm time uuid")
        {
            chars.insert(StimulusKey::AlarmTime, characteristic);
        } else if characteristic.uuid
            == Uuid::parse_str(ALARM_CONTROL_UUID).expect("valid alarm control uuid")
        {
            chars.insert(StimulusKey::AlarmControl, characteristic);
        } else if characteristic.uuid
            == Uuid::parse_str(ALARM_WRITE_UUID).expect("valid alarm write uuid")
        {
            chars.insert(StimulusKey::AlarmWrite, characteristic);
        } else if characteristic.uuid == Uuid::parse_str(SETUP_UUID).expect("valid setup uuid") {
            chars.insert(StimulusKey::Setup, characteristic);
        }
    }
    if !chars.contains_key(&StimulusKey::Vibe) || !chars.contains_key(&StimulusKey::Beep) {
        bail!("Connected, but Pavlok vibe/beep characteristics were not found.");
    }
    Ok(PavlokClient {
        peripheral,
        chars,
        allow_zap,
    })
}

impl PavlokClient {
    async fn send(&self, request: StimRequest, trigger_started: Instant) -> Result<SendBody> {
        if request.stim == Stimulus::Zap && !self.allow_zap {
            bail!("Zap is disabled. Restart with --allow-zap only when you intend to zap.");
        }
        let payload = payload_for(&request);
        let result = self
            .write_payload(
                StimulusKey::from(request.stim),
                &payload,
                request.mode,
                trigger_started,
            )
            .await
            .with_context(|| format!("write {} stimulus", request.stim.as_str()))?;
        Ok(SendBody {
            ok: true,
            stim: request.stim.as_str(),
            mode: request.mode.as_str(),
            payload_hex: result.payload_hex,
            timing_ms: result.timing_ms,
        })
    }

    async fn send_webtool(
        &self,
        command: WebtoolCommand,
        trigger_started: Instant,
    ) -> Result<WebtoolBody> {
        if command.requires_zap && !self.allow_zap {
            bail!("Zap is disabled. Restart with --allow-zap only when you intend to zap.");
        }
        let mut characteristic_names = Vec::with_capacity(command.writes.len());
        let mut payload_hexes = Vec::with_capacity(command.writes.len());
        let mut timing = Timing::default();
        for write in &command.writes {
            let result = self
                .write_payload(
                    write.characteristic,
                    &write.payload,
                    command.mode,
                    trigger_started,
                )
                .await
                .with_context(|| format!("write Webtool {}", command.name))?;
            characteristic_names.push(write.characteristic.as_str());
            payload_hexes.push(result.payload_hex);
            merge_write_timing(&mut timing, result.timing_ms);
        }
        Ok(WebtoolBody {
            ok: true,
            command: command.name,
            characteristic: characteristic_names.join(","),
            mode: command.mode.as_str(),
            payload_hex: payload_hexes.join(";"),
            timing_ms: timing,
        })
    }

    async fn schedule_vibe_alarm(
        &self,
        request: VibeAlarmRequest,
        trigger_started: Instant,
    ) -> Result<AlarmScheduleBody> {
        let alarm_time =
            local_alarm_time(request.minutes).context("read local target alarm time")?;
        let alarm_id = alarm_id_from_time(&local_date_parts(0)?);
        let entry = AlarmEntry {
            name: request.name,
            stim: Stimulus::Vibe,
            hour: alarm_time.hour,
            minute: alarm_time.minute,
            alarm_id,
            intensity: request.intensity,
            count: request.count,
            active: true,
        };
        let result = self
            .write_alarm_entries("schedule", "Single 1", &[entry], trigger_started)
            .await?;
        let alarm = result
            .alarms
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("alarm write returned no alarms"))?;

        Ok(AlarmScheduleBody {
            ok: true,
            name: alarm.name,
            scheduled_time: alarm.scheduled_time,
            scheduled_hour: alarm.scheduled_hour,
            scheduled_minute: alarm.scheduled_minute,
            alarm_id: alarm.alarm_id,
            payload_hex: result.payload_hex,
            chunks: result.chunks,
            next_alarm: result.next_alarm,
            timing_ms: result.timing_ms,
        })
    }

    async fn write_alarm_entries(
        &self,
        action: &'static str,
        profile: &str,
        entries: &[AlarmEntry],
        trigger_started: Instant,
    ) -> Result<AlarmBatchBody> {
        if entries.is_empty() {
            bail!("alarm write needs at least one entry");
        }
        if entries
            .iter()
            .any(|entry| entry.stim == Stimulus::Zap && !self.allow_zap)
        {
            bail!("Zap alarms are disabled. Restart with --allow-zap only when you intend to zap.");
        }

        let now = local_date_parts(0).context("read local time for bracelet sync")?;
        let time_payload = pavlok_time_payload(&now);
        let alarm_payload = build_alarm_file(profile, entries);

        let mut timing = Timing::default();
        let time_result = self
            .write_payload(
                StimulusKey::Time,
                &time_payload,
                WriteMode::Response,
                trigger_started,
            )
            .await
            .context("write bracelet time")?;
        merge_write_timing(&mut timing, time_result.timing_ms);

        if let Some(characteristic) = self.chars.get(&StimulusKey::AlarmWrite)
            && characteristic.properties.contains(CharPropFlags::NOTIFY)
        {
            self.peripheral
                .subscribe(characteristic)
                .await
                .context("subscribe to alarm write notifications")?;
        }

        let start_result = self
            .write_payload(
                StimulusKey::AlarmControl,
                &[1],
                WriteMode::Response,
                trigger_started,
            )
            .await
            .context("begin alarm write")?;
        merge_write_timing(&mut timing, start_result.timing_ms);
        sleep(Duration::from_millis(100)).await;

        for chunk in alarm_payload.chunks(20) {
            let chunk_result = self
                .write_payload(
                    StimulusKey::AlarmWrite,
                    chunk,
                    WriteMode::NoResponse,
                    trigger_started,
                )
                .await
                .context("write alarm payload chunk")?;
            merge_write_timing(&mut timing, chunk_result.timing_ms);
        }
        sleep(Duration::from_secs(2)).await;

        let finish_result = self
            .write_payload(
                StimulusKey::AlarmControl,
                &[0],
                WriteMode::Response,
                trigger_started,
            )
            .await
            .context("finish alarm write")?;
        merge_write_timing(&mut timing, finish_result.timing_ms);

        sleep(Duration::from_millis(300)).await;
        let next_alarm = self
            .read_payload(StimulusKey::AlarmTime)
            .await
            .ok()
            .and_then(|payload| parse_next_alarm(&payload));

        Ok(AlarmBatchBody {
            ok: true,
            action,
            profile: profile.to_string(),
            alarms: entries.iter().map(AlarmEntry::record).collect(),
            payload_hex: hex(&alarm_payload),
            chunks: alarm_payload.len().div_ceil(20),
            next_alarm,
            state_file: wakeup_alarm_state_path().display().to_string(),
            timing_ms: timing,
        })
    }

    async fn read_payload(&self, characteristic_key: StimulusKey) -> Result<Vec<u8>> {
        let characteristic = self
            .chars
            .get(&characteristic_key)
            .ok_or_else(|| anyhow!("Missing {} characteristic.", characteristic_key.as_str()))?;
        if !characteristic.properties.contains(CharPropFlags::READ) {
            bail!("{} does not advertise read.", characteristic_key.as_str());
        }
        let bytes = timeout(BLE_WRITE_TIMEOUT, self.peripheral.read(characteristic))
            .await
            .with_context(|| format!("read {} timed out", characteristic_key.as_str()))?
            .with_context(|| format!("read {}", characteristic_key.as_str()))?;
        Ok(bytes)
    }

    async fn write_payload(
        &self,
        characteristic_key: StimulusKey,
        payload: &[u8],
        mode: WriteMode,
        trigger_started: Instant,
    ) -> Result<WriteResult> {
        let characteristic = self
            .chars
            .get(&characteristic_key)
            .ok_or_else(|| anyhow!("Missing {} characteristic.", characteristic_key.as_str()))?;
        validate_write_mode(characteristic, mode, characteristic_key.as_str())?;

        let write_started = Instant::now();
        timeout(
            BLE_WRITE_TIMEOUT,
            self.peripheral
                .write(characteristic, payload, mode.btleplug()),
        )
        .await
        .with_context(|| format!("write {} timed out", characteristic_key.as_str()))?
        .with_context(|| format!("write {}", characteristic_key.as_str()))?;
        let ack_ms = ms_since(trigger_started);
        let timing = Timing {
            trigger_to_write_issued: Some(ms_between(trigger_started, write_started)),
            write_call_wall: Some(ms_since(write_started)),
            trigger_to_write_ack: (mode == WriteMode::Response).then_some(ack_ms),
            request_to_response: None,
        };
        Ok(WriteResult {
            payload_hex: hex(payload),
            timing_ms: timing,
        })
    }
}

fn validate_write_mode(
    characteristic: &Characteristic,
    mode: WriteMode,
    label: &str,
) -> Result<()> {
    match mode {
        WriteMode::Response if !characteristic.properties.contains(CharPropFlags::WRITE) => {
            bail!("{label} does not advertise writeWithResponse.")
        }
        WriteMode::NoResponse
            if !characteristic
                .properties
                .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) =>
        {
            bail!("{label} does not advertise writeWithoutResponse.")
        }
        _ => Ok(()),
    }
}

struct HttpRequest {
    method: String,
    target: String,
    body: String,
}

struct HttpHead {
    method: String,
    target: String,
    body_start: usize,
    content_length: usize,
}

const MAX_HTTP_BYTES: usize = 64 * 1024;

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0; 4096];
    loop {
        if let Some(head) = parse_http_head(&buffer)? {
            let total_len = head.body_start + head.content_length;
            if total_len > MAX_HTTP_BYTES {
                bail!("HTTP request too large");
            }
            while buffer.len() < total_len {
                let n = stream
                    .read(&mut chunk)
                    .await
                    .context("read HTTP request body")?;
                if n == 0 {
                    bail!("HTTP body ended before Content-Length");
                }
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() > MAX_HTTP_BYTES {
                    bail!("HTTP request too large");
                }
            }
            let body = String::from_utf8_lossy(&buffer[head.body_start..total_len]).into_owned();
            return Ok(HttpRequest {
                method: head.method,
                target: head.target,
                body,
            });
        }

        let n = stream.read(&mut chunk).await.context("read HTTP request")?;
        if n == 0 {
            bail!("HTTP request ended before headers");
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > MAX_HTTP_BYTES {
            bail!("HTTP request too large");
        }
    }
}

fn parse_http_head(buffer: &[u8]) -> Result<Option<HttpHead>> {
    let Some((headers_end, delimiter_len)) = find_header_end(buffer) else {
        return Ok(None);
    };
    let headers = String::from_utf8_lossy(&buffer[..headers_end]);
    let mut lines = headers.lines();
    let first = lines.next().unwrap_or_default();
    let parts: Vec<&str> = first.split_whitespace().collect();
    if parts.len() < 2 {
        bail!("bad request");
    }
    let mut content_length = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .context("parse Content-Length")?;
        }
    }
    Ok(Some(HttpHead {
        method: parts[0].to_string(),
        target: parts[1].to_string(),
        body_start: headers_end + delimiter_len,
        content_length,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        })
}

async fn serve(args: ServeArgs, client: Arc<Mutex<PavlokClient>>) -> Result<()> {
    let listener = TcpListener::bind((&*args.host, args.port))
        .await
        .with_context(|| format!("bind {}:{}", args.host, args.port))?;
    println!("listening http://{}:{}", args.host, args.port);
    loop {
        let (stream, _) = listener.accept().await.context("accept HTTP connection")?;
        let client = Arc::clone(&client);
        let defaults = args.common.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_http(stream, client, defaults).await {
                eprintln!("http error: {error:#}");
            }
        });
    }
}

async fn handle_http(
    mut stream: TcpStream,
    client: Arc<Mutex<PavlokClient>>,
    defaults: CommonArgs,
) -> Result<()> {
    let request_started = Instant::now();
    let request = match timeout(HTTP_REQUEST_TIMEOUT, read_http_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            return write_response(
                &mut stream,
                400,
                ErrorBody {
                    ok: false,
                    error: error.to_string(),
                    timing_ms: timing_with_response(request_started),
                },
            )
            .await;
        }
        Err(_) => {
            return write_response(
                &mut stream,
                408,
                ErrorBody {
                    ok: false,
                    error: "HTTP request timed out".to_string(),
                    timing_ms: timing_with_response(request_started),
                },
            )
            .await;
        }
    };

    let method = request.method.as_str();
    let target = request.target.as_str();
    let (path, query) = parse_target(target);
    let body = request.body.as_str();
    if method == "GET" && path == "/health" {
        let connected = timeout(HTTP_REQUEST_TIMEOUT, async {
            client
                .lock()
                .await
                .peripheral
                .is_connected()
                .await
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        return write_response(
            &mut stream,
            200,
            HealthBody {
                ok: true,
                connected,
                timing_ms: timing_with_response(request_started),
            },
        )
        .await;
    }

    if method == "POST" && path == "/api/v5/stimulus/send" {
        let stimulus_request = match request_from_cloud_body(body, &defaults) {
            Ok(request) => request,
            Err(error) => {
                return write_response(
                    &mut stream,
                    422,
                    ErrorBody {
                        ok: false,
                        error,
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await;
            }
        };
        let result = timeout(HTTP_ACTION_TIMEOUT, async {
            client
                .lock()
                .await
                .send(stimulus_request, request_started)
                .await
        })
        .await
        .context("stimulus request timed out");
        return match result {
            Ok(Ok(mut body)) => {
                body.timing_ms.request_to_response = Some(ms_since(request_started));
                write_response(&mut stream, 200, body).await
            }
            Ok(Err(error)) | Err(error) => {
                write_response(
                    &mut stream,
                    500,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await
            }
        };
    }

    if method == "POST" && path.starts_with("/webtool/") {
        let command = match webtool_command_from_route(&path, &query, &defaults) {
            Ok(command) => command,
            Err(error) => {
                return write_response(
                    &mut stream,
                    422,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await;
            }
        };
        let result = timeout(HTTP_ACTION_TIMEOUT, async {
            client
                .lock()
                .await
                .send_webtool(command, request_started)
                .await
        })
        .await
        .context("Webtool request timed out");
        return match result {
            Ok(Ok(mut body)) => {
                body.timing_ms.request_to_response = Some(ms_since(request_started));
                write_response(&mut stream, 200, body).await
            }
            Ok(Err(error)) | Err(error) => {
                write_response(
                    &mut stream,
                    500,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await
            }
        };
    }

    if method == "POST" && path == "/alarm/vibe" {
        let minutes = get_u16(&query, "minutes", 1, 1, 1440);
        let request = VibeAlarmRequest {
            minutes,
            intensity: get_u8(&query, "intensity", defaults.intensity, 1, 100),
            count: get_u8(&query, "count", defaults.count, 1, 7),
            name: query
                .get("name")
                .map(|value| decode_query_value(value))
                .unwrap_or_else(|| "Codex demo".to_string()),
        };
        let result = timeout(Duration::from_secs(20), async {
            client
                .lock()
                .await
                .schedule_vibe_alarm(request, request_started)
                .await
        })
        .await
        .context("alarm write timed out");
        return match result {
            Ok(Ok(mut body)) => {
                body.timing_ms.request_to_response = Some(ms_since(request_started));
                write_response(&mut stream, 200, body).await
            }
            Ok(Err(error)) | Err(error) => {
                write_response(
                    &mut stream,
                    500,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await
            }
        };
    }

    if method == "POST" && path == "/alarm/wakeup/schedule" {
        let minutes = get_u16(&query, "minutes", 1, 1, 1440);
        let entries = get_u8(&query, "entries", 6, 1, 6);
        let intensity = get_u8(&query, "intensity", defaults.intensity, 1, 100);
        let prefix = query
            .get("name")
            .map(|value| decode_query_value(value))
            .unwrap_or_else(|| "Wake".to_string());
        let alarms = wakeup_alarm_entries(minutes, entries, intensity, &prefix, true)?;
        let state = WakeupAlarmState {
            profile: WAKEUP_ALARM_PROFILE.to_string(),
            alarms: alarms.iter().map(AlarmEntry::record).collect(),
        };
        let result = timeout(Duration::from_secs(30), async {
            client
                .lock()
                .await
                .write_alarm_entries("schedule", WAKEUP_ALARM_PROFILE, &alarms, request_started)
                .await
        })
        .await
        .context("wake-up alarm write timed out");
        return match result {
            Ok(Ok(mut body)) => {
                body.timing_ms.request_to_response = Some(ms_since(request_started));
                match save_wakeup_alarm_state(&state) {
                    Ok(()) => write_response(&mut stream, 200, body).await,
                    Err(error) => {
                        write_response(
                            &mut stream,
                            500,
                            ErrorBody {
                                ok: false,
                                error: error.to_string(),
                                timing_ms: timing_with_response(request_started),
                            },
                        )
                        .await
                    }
                }
            }
            Ok(Err(error)) | Err(error) => {
                write_response(
                    &mut stream,
                    500,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await
            }
        };
    }

    if method == "POST" && path == "/alarm/wakeup/cancel" {
        let state = match load_wakeup_alarm_state() {
            Ok(Some(state)) => state,
            Ok(None) => WakeupAlarmState {
                profile: WAKEUP_ALARM_PROFILE.to_string(),
                alarms: wakeup_alarm_entries(
                    get_u16(&query, "minutes", 1, 1, 1440),
                    get_u8(&query, "entries", 6, 1, 6),
                    get_u8(&query, "intensity", defaults.intensity, 1, 100),
                    "Wake",
                    true,
                )?
                .iter()
                .map(AlarmEntry::record)
                .collect(),
            },
            Err(error) => {
                return write_response(
                    &mut stream,
                    500,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await;
            }
        };
        let disabled = state
            .alarms
            .iter()
            .map(|alarm| AlarmEntry {
                name: alarm.name.clone(),
                stim: alarm.stim,
                hour: alarm.scheduled_hour,
                minute: alarm.scheduled_minute,
                alarm_id: alarm.alarm_id,
                intensity: alarm.intensity,
                count: alarm.count,
                active: false,
            })
            .collect::<Vec<_>>();
        let result = timeout(Duration::from_secs(30), async {
            client
                .lock()
                .await
                .write_alarm_entries("cancel", &state.profile, &disabled, request_started)
                .await
        })
        .await
        .context("wake-up alarm cancel timed out");
        return match result {
            Ok(Ok(mut body)) => {
                body.timing_ms.request_to_response = Some(ms_since(request_started));
                remove_wakeup_alarm_state().ok();
                write_response(&mut stream, 200, body).await
            }
            Ok(Err(error)) | Err(error) => {
                write_response(
                    &mut stream,
                    500,
                    ErrorBody {
                        ok: false,
                        error: error.to_string(),
                        timing_ms: timing_with_response(request_started),
                    },
                )
                .await
            }
        };
    }

    if method != "POST" || !path.starts_with("/stim/") {
        return write_response(
            &mut stream,
            404,
            ErrorBody {
                ok: false,
                error:
                    "use POST /api/v5/stimulus/send, /stim/vibe, /stim/beep, /alarm/vibe, /alarm/wakeup/schedule, /alarm/wakeup/cancel, or /webtool/testVibe"
                        .to_string(),
                timing_ms: timing_with_response(request_started),
            },
        )
        .await;
    }

    let stim = match <Stimulus as FromStr>::from_str(path.trim_start_matches("/stim/")) {
        Ok(stim) => stim,
        Err(error) => {
            return write_response(
                &mut stream,
                400,
                ErrorBody {
                    ok: false,
                    error: error.to_string(),
                    timing_ms: timing_with_response(request_started),
                },
            )
            .await;
        }
    };
    let request = request_from_query(stim, &query, &defaults);
    let result = timeout(HTTP_ACTION_TIMEOUT, async {
        client.lock().await.send(request, request_started).await
    })
    .await
    .context("stimulus request timed out");
    match result {
        Ok(Ok(mut body)) => {
            body.timing_ms.request_to_response = Some(ms_since(request_started));
            write_response(&mut stream, 200, body).await
        }
        Ok(Err(error)) | Err(error) => {
            write_response(
                &mut stream,
                500,
                ErrorBody {
                    ok: false,
                    error: error.to_string(),
                    timing_ms: timing_with_response(request_started),
                },
            )
            .await
        }
    }
}

async fn write_response<T: Serialize>(stream: &mut TcpStream, status: u16, body: T) -> Result<()> {
    let body = serde_json::to_vec(&body).context("serialize JSON")?;
    let status_text = if status < 400 { "OK" } else { "Error" };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .await
        .context("write HTTP header")?;
    stream.write_all(&body).await.context("write HTTP body")?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn run_stdin(args: CommonArgs, client: PavlokClient) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("read stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        match request_from_line(&line, &args) {
            Ok(request) => match client.send(request, Instant::now()).await {
                Ok(body) => print_json(&body)?,
                Err(error) => {
                    print_json(&serde_json::json!({"ok": false, "error": error.to_string()}))?
                }
            },
            Err(error) => {
                print_json(&serde_json::json!({"ok": false, "error": error.to_string()}))?
            }
        }
    }
    Ok(())
}

struct ScreenTimeThresholdMonitor {
    last_checked_minute: Option<String>,
    window_label: Option<String>,
    last_fired_minutes: HashMap<&'static str, u64>,
}

impl ScreenTimeThresholdMonitor {
    fn new() -> Self {
        Self {
            last_checked_minute: None,
            window_label: None,
            last_fired_minutes: HashMap::new(),
        }
    }
}

struct ViolationZapCounters {
    path: Option<PathBuf>,
    state: ViolationZapCounterState,
}

#[derive(Default, Deserialize, Serialize)]
struct ViolationZapCounterState {
    window_label: Option<String>,
    counts: HashMap<String, u64>,
}

impl ViolationZapCounters {
    fn new() -> Self {
        let path = violation_zap_counts_path();
        let state = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default();
        Self { path, state }
    }

    fn next_count(&mut self, key: &str) -> Result<u64> {
        let window = current_screen_time_window()?;
        let count = self.next_count_for_window(key, &window.window_label);
        self.save().context("save violation zap counts")?;
        Ok(count)
    }

    fn next_count_for_window(&mut self, key: &str, window_label: &str) -> u64 {
        if self.state.window_label.as_deref() != Some(window_label) {
            self.state.window_label = Some(window_label.to_string());
            self.state.counts.clear();
        }

        let count = self.state.counts.entry(key.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let contents =
            serde_json::to_string_pretty(&self.state).context("serialize violation zap counts")?;
        std::fs::write(path, format!("{contents}\n"))
            .with_context(|| format!("write {}", path.display()))
    }
}

fn violation_zap_counts_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && !matches!(home.to_str(), Some("") | Some("/") | Some("/var/empty"))
    {
        return Some(home.join(".config/pavlov/violation-counts.json"));
    }

    let username = run_command("/usr/bin/id", &["-un"]).ok()?;
    Some(PathBuf::from(format!(
        "/Users/{username}/.config/pavlov/violation-counts.json"
    )))
}

fn wakeup_alarm_state_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && !matches!(home.to_str(), Some("") | Some("/") | Some("/var/empty"))
    {
        return home.join(".config/pavlov/wakeup-alarms.json");
    }

    let username = run_command("/usr/bin/id", &["-un"]).unwrap_or_else(|_| "andrewzabelo".into());
    PathBuf::from(format!(
        "/Users/{username}/.config/pavlov/wakeup-alarms.json"
    ))
}

fn save_wakeup_alarm_state(state: &WakeupAlarmState) -> Result<()> {
    let path = wakeup_alarm_state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(state).context("serialize wake-up alarm state")?;
    std::fs::write(&path, format!("{contents}\n"))
        .with_context(|| format!("write {}", path.display()))
}

fn load_wakeup_alarm_state() -> Result<Option<WakeupAlarmState>> {
    let path = wakeup_alarm_state_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .with_context(|| format!("parse {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn remove_wakeup_alarm_state() -> Result<()> {
    let path = wakeup_alarm_state_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

struct ScreenTimeThresholdEvent {
    app_name: &'static str,
    usage_key: &'static str,
    total_seconds: u64,
    threshold_minutes: u64,
}

struct ScreenTimeWindow {
    minute_label: String,
    window_label: String,
    apple_reference_start: i64,
}

fn current_screen_time_window() -> Result<ScreenTimeWindow> {
    let minute_label = run_command("/bin/date", &["+%Y-%m-%d %H:%M"])
        .context("read current local minute for screen time")?;
    let window_label = run_command("/bin/sh", &["-c", "if [ \"$(/bin/date +%H)\" -lt 5 ]; then /bin/date -v-1d +%Y-%m-%d; else /bin/date +%Y-%m-%d; fi"])
        .context("read screen time window label")?;
    let unix_start = run_command(
        "/bin/sh",
        &[
            "-c",
            "if [ \"$(/bin/date +%H)\" -lt 5 ]; then /bin/date -v-1d -v5H -v0M -v0S +%s; else /bin/date -v5H -v0M -v0S +%s; fi",
        ],
    )
    .context("read screen time window start")?
    .parse::<i64>()
    .context("parse screen time window start seconds")?;
    Ok(ScreenTimeWindow {
        minute_label,
        window_label,
        apple_reference_start: unix_start - 978_307_200,
    })
}

fn screen_time_seconds_since(window: &ScreenTimeWindow, bundle_id: &str) -> Result<u64> {
    let query = format!(
        "select coalesce(sum(case when ZENDDATE > {start} then ZENDDATE - max(ZSTARTDATE, {start}) else 0 end), 0) from ZOBJECT where ZSTREAMNAME='/app/usage' and ZVALUESTRING='{bundle}' and ZENDDATE > {start};",
        start = window.apple_reference_start,
        bundle = bundle_id.replace('\'', "''"),
    );
    let output = run_command(
        "/usr/bin/sqlite3",
        &["-readonly", screen_time_db_path(), &query],
    )
    .with_context(|| format!("query screen time usage for {bundle_id}"))?;
    let seconds = output.trim().parse::<f64>().unwrap_or(0.0).max(0.0);
    Ok(seconds.floor() as u64)
}

fn screen_time_youtube_seconds_since(window: &ScreenTimeWindow) -> Result<u64> {
    let query = format!(
        "\
select coalesce(sum(case when o.ZENDDATE > {start} then o.ZENDDATE - max(o.ZSTARTDATE, {start}) else 0 end), 0) \
from ZOBJECT o \
left join ZSTRUCTUREDMETADATA m on o.ZSTRUCTUREDMETADATA = m.Z_PK \
where o.ZSTREAMNAME = '/app/webUsage' \
and o.ZENDDATE > {start} \
and {youtube_filter};",
        start = window.apple_reference_start,
        youtube_filter = youtube_screen_time_sql_filter(),
    );
    let output = run_command(
        "/usr/bin/sqlite3",
        &["-readonly", screen_time_db_path(), &query],
    )
    .context("query screen time usage for YouTube web domains")?;
    let seconds = output.trim().parse::<f64>().unwrap_or(0.0).max(0.0);
    Ok(seconds.floor() as u64)
}

fn youtube_screen_time_sql_filter() -> &'static str {
    "\
(lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN, '')) = 'youtube.com' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN, '')) like '%.youtube.com' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN, '')) = 'youtu.be' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBDOMAIN, '')) like '%.youtu.be' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBPAGEURL, '')) like '%://youtube.com/%' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBPAGEURL, '')) like '%://www.youtube.com/%' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBPAGEURL, '')) like '%://m.youtube.com/%' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBPAGEURL, '')) like '%://music.youtube.com/%' \
or lower(coalesce(m.Z_DKDIGITALHEALTHMETADATAKEY__WEBPAGEURL, '')) like '%://youtu.be/%')"
}

fn screen_time_threshold_minutes(total_seconds: u64) -> Option<u64> {
    if total_seconds < 20 * 60 {
        None
    } else {
        Some(20 + ((total_seconds - 20 * 60) / (5 * 60)) * 5)
    }
}

fn screen_time_db_path() -> &'static str {
    "/Users/andrewzabelo/Library/Application Support/Knowledge/knowledgeC.db"
}

fn check_screen_time_thresholds(
    monitor: &mut ScreenTimeThresholdMonitor,
) -> Result<Vec<ScreenTimeThresholdEvent>> {
    let window = current_screen_time_window()?;
    if monitor.last_checked_minute.as_deref() == Some(window.minute_label.as_str()) {
        return Ok(Vec::new());
    }
    monitor.last_checked_minute = Some(window.minute_label.clone());
    if monitor.window_label.as_deref() != Some(window.window_label.as_str()) {
        monitor.window_label = Some(window.window_label.clone());
        monitor.last_fired_minutes.clear();
    }

    let mut events = Vec::new();
    for usage in [
        ("Messages", "com.apple.MobileSMS"),
        ("Outlook", "com.microsoft.Outlook"),
    ]
    .into_iter()
    .map(|(app_name, bundle_id)| {
        screen_time_seconds_since(&window, bundle_id).map(|seconds| (app_name, bundle_id, seconds))
    })
    .chain(std::iter::once(
        screen_time_youtube_seconds_since(&window)
            .map(|seconds| ("YouTube", "web:youtube", seconds)),
    )) {
        let (app_name, usage_key, total_seconds) = usage?;
        let Some(threshold_minutes) = screen_time_threshold_minutes(total_seconds) else {
            continue;
        };
        let last_fired = monitor
            .last_fired_minutes
            .get(usage_key)
            .copied()
            .unwrap_or(0);
        if threshold_minutes > last_fired {
            monitor
                .last_fired_minutes
                .insert(usage_key, threshold_minutes);
            events.push(ScreenTimeThresholdEvent {
                app_name,
                usage_key,
                total_seconds,
                threshold_minutes,
            });
        }
    }
    Ok(events)
}

async fn emit_monitor_zap(
    args: &MonitorArgs,
    client: Option<&PavlokClient>,
    signal_name: &str,
    reason: &str,
    zap_count: u64,
    context: serde_json::Value,
) -> Result<()> {
    let body = serde_json::json!({
        "ok": true,
        "signal": signal_name,
        "reason": reason,
        "dry_run": args.dry_run,
        "zap_count": zap_count,
        "context": context,
    });
    print_json(&body)?;

    if let Some(client) = client {
        for zap_index in 1..=zap_count {
            match client
                .send(default_request(&args.common, Stimulus::Zap), Instant::now())
                .await
            {
                Ok(body) => {
                    let notification_requested = notify_zap_sent(reason);
                    print_json(&serde_json::json!({
                        "ok": true,
                        "stim": body.stim,
                        "mode": body.mode,
                        "payload_hex": body.payload_hex,
                        "timing_ms": body.timing_ms,
                        "notification_requested": notification_requested,
                        "zap_index": zap_index,
                        "zap_count": zap_count,
                    }))?
                }
                Err(error) => {
                    print_json(&serde_json::json!({
                        "ok": false,
                        "signal": signal_name,
                        "error": error.to_string(),
                        "zap_index": zap_index,
                        "zap_count": zap_count,
                    }))?;
                }
            }
            sleep_between_monitor_zaps(zap_index, zap_count).await;
        }
    } else if !args.dry_run {
        for zap_index in 1..=zap_count {
            match send_zap_http(&args.zap_url, &args.common).await {
                Ok(response) => {
                    let notification_requested = notify_zap_sent(reason);
                    print_json(&serde_json::json!({
                        "ok": true,
                        "stim": "zap",
                        "via": "http",
                        "zap_url": args.zap_url,
                        "response": response,
                        "notification_requested": notification_requested,
                        "zap_index": zap_index,
                        "zap_count": zap_count,
                    }))?
                }
                Err(error) => print_json(&serde_json::json!({
                    "ok": false,
                    "stim": "zap",
                    "via": "http",
                    "zap_url": args.zap_url,
                    "error": error.to_string(),
                    "zap_index": zap_index,
                    "zap_count": zap_count,
                }))?,
            }
            sleep_between_monitor_zaps(zap_index, zap_count).await;
        }
    }

    Ok(())
}

async fn sleep_between_monitor_zaps(zap_index: u64, zap_count: u64) {
    if zap_index < zap_count {
        sleep(Duration::from_millis(250)).await;
    }
}

async fn run_monitor(
    args: MonitorArgs,
    client: Option<PavlokClient>,
    config: SignalConfig,
) -> Result<()> {
    let mut cooldowns = SignalCooldowns::new(Duration::from_secs(args.cooldown_secs));
    let mut warnings_seen = std::collections::HashSet::new();
    let mut screen_time_monitor = ScreenTimeThresholdMonitor::new();
    let mut violation_zap_counters = ViolationZapCounters::new();
    let mut active_one_shot_violations = std::collections::HashSet::new();
    let mut suppress_initial_one_shot_violations = true;
    loop {
        let (snapshot, warnings) = signals::read_snapshot(args.dnd_command.as_deref());
        for warning in warnings {
            if warnings_seen.insert(warning.clone()) {
                eprintln!("monitor warning: {warning}");
            }
        }

        let mut current_one_shot_violations = std::collections::HashSet::new();
        for violation in config.violations(&snapshot) {
            if let Some(one_shot_key) = violation.one_shot_key() {
                current_one_shot_violations.insert(one_shot_key.to_string());
                if suppress_initial_one_shot_violations {
                    active_one_shot_violations.insert(one_shot_key.to_string());
                    continue;
                }
                if active_one_shot_violations.contains(one_shot_key) {
                    continue;
                }
                active_one_shot_violations.insert(one_shot_key.to_string());
            }
            if !cooldowns.ready(violation.cooldown_key(), Instant::now()) {
                continue;
            }
            let reason = signal_notification_reason(&violation);
            let counter_key = violation.cooldown_key();
            let zap_count = violation_zap_counters.next_count(&counter_key)?;
            emit_monitor_zap(
                &args,
                client.as_ref(),
                violation.signal_name(),
                &reason,
                zap_count,
                serde_json::json!({
                    "rule": violation.rule,
                    "frontmost_app": snapshot.frontmost_app,
                    "browser_url": snapshot.browser_url,
                    "browser_title": snapshot.browser_title,
                    "dnd_enabled": snapshot.dnd_enabled,
                    "source": violation.source,
                    "violation_count_key": counter_key,
                    "violation_count_since_5am": zap_count,
                }),
            )
            .await?;
        }
        active_one_shot_violations.retain(|key| current_one_shot_violations.contains(key));
        suppress_initial_one_shot_violations = false;

        match check_screen_time_thresholds(&mut screen_time_monitor) {
            Ok(events) => {
                for event in events {
                    let reason = format!(
                        "{} Screen Time reached {} minutes since 5:00 AM",
                        event.app_name, event.threshold_minutes
                    );
                    let counter_key = format!("screen-time:{}", event.usage_key);
                    let zap_count = violation_zap_counters.next_count(&counter_key)?;
                    emit_monitor_zap(
                        &args,
                        client.as_ref(),
                        "screen_time_threshold",
                        &reason,
                        zap_count,
                        serde_json::json!({
                            "app_name": event.app_name,
                            "usage_key": event.usage_key,
                            "threshold_minutes": event.threshold_minutes,
                            "total_seconds": event.total_seconds,
                            "window_starts_at": "05:00",
                            "violation_count_key": counter_key,
                            "violation_count_since_5am": zap_count,
                        }),
                    )
                    .await?;
                }
            }
            Err(error) => {
                let warning = format!("screen time query failed: {error}");
                if warnings_seen.insert(warning.clone()) {
                    eprintln!("monitor warning: {warning}");
                }
            }
        }

        sleep(Duration::from_millis(args.poll_ms)).await;
    }
}

fn signal_notification_reason(violation: &signals::SignalViolation<'_>) -> String {
    match (violation.rule.kind, violation.rule.scope) {
        (SignalRuleKind::App, RuleScope::Always) => {
            format!("{} opened", violation.rule.pattern)
        }
        (SignalRuleKind::App, RuleScope::Dnd) => {
            format!("{} opened during Do Not Disturb", violation.rule.pattern)
        }
        (SignalRuleKind::Website, RuleScope::Always) => {
            format!("Website matched {}", violation.rule.pattern)
        }
        (SignalRuleKind::Website, RuleScope::Dnd) => {
            format!(
                "Website matched {} during Do Not Disturb",
                violation.rule.pattern
            )
        }
    }
}

fn run_command(program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn notify_zap_sent(reason: &str) -> bool {
    let script = format!(
        r#"display notification "{}" with title "Pavlov zap sent""#,
        escape_osascript_string(reason)
    );
    match std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .status()
    {
        Ok(status) => status.success(),
        Err(error) => {
            eprintln!("notification error: {error}");
            false
        }
    }
}

fn escape_osascript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn send_zap_http(url: &str, defaults: &CommonArgs) -> Result<String> {
    let parsed = parse_http_url(url)?;
    let target = target_with_stim_defaults(&parsed.target, defaults);
    let mut stream = timeout(
        Duration::from_secs(5),
        TcpStream::connect((parsed.host.as_str(), parsed.port)),
    )
    .await
    .context("zap HTTP connect timed out")?
    .with_context(|| format!("connect {}:{}", parsed.host, parsed.port))?;
    let request = format!(
        "POST {target} HTTP/1.1\r\nHost: {}:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        parsed.host, parsed.port
    );
    timeout(Duration::from_secs(5), stream.write_all(request.as_bytes()))
        .await
        .context("zap HTTP write timed out")?
        .context("write zap HTTP request")?;

    let mut response = String::new();
    timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .context("zap HTTP response timed out")?
        .context("read zap HTTP response")?;
    let status_line = response.lines().next().unwrap_or_default();
    if status_line.contains(" 2") {
        Ok(response
            .split_once("\r\n\r\n")
            .or_else(|| response.split_once("\n\n"))
            .map(|(_, body)| body.trim().to_string())
            .unwrap_or_default())
    } else {
        bail!(
            "zap HTTP request failed: {}",
            response.lines().take(1).collect::<Vec<_>>().join("")
        )
    }
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    target: String,
}

fn parse_http_url(url: &str) -> Result<ParsedHttpUrl> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("only http:// zap URLs are supported"))?;
    let (host_port, target) = rest.split_once('/').unwrap_or((rest, ""));
    let target = format!("/{target}");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().context("parse zap URL port")?),
        None => (host_port, 80),
    };
    if host.is_empty() {
        bail!("zap URL host cannot be empty");
    }
    Ok(ParsedHttpUrl {
        host: host.to_string(),
        port,
        target,
    })
}

fn target_with_stim_defaults(target: &str, defaults: &CommonArgs) -> String {
    let separator = if target.contains('?') { '&' } else { '?' };
    format!(
        "{target}{separator}intensity={}&count={}&on_ms={}&off_ms={}&mode={}",
        defaults.intensity.clamp(1, 100),
        defaults.count.clamp(1, 7),
        defaults.on_ms.max(1),
        defaults.off_ms.max(1),
        defaults.mode,
    )
}

fn run_rules(args: RulesArgs) -> Result<()> {
    let path = SignalConfig::path(args.rules_file.as_deref())?;
    match args.command {
        RulesCommand::Path => {
            println!("{}", path.display());
            Ok(())
        }
        RulesCommand::List => {
            let config = SignalConfig::load(args.rules_file.as_deref())?;
            println!("rules_file {}", path.display());
            for rule in &config.rules {
                println!("{} {} {}", rule.kind, rule.scope, rule.pattern);
            }
            Ok(())
        }
        RulesCommand::AddApp { name, scope } => {
            let mut config = SignalConfig::load(args.rules_file.as_deref())?;
            let added = config.add_rule(SignalRule::new(SignalRuleKind::App, scope, name)?);
            config.save(args.rules_file.as_deref())?;
            println!(
                "{} app rule in {}",
                if added { "added" } else { "already had" },
                path.display()
            );
            Ok(())
        }
        RulesCommand::AddSite { pattern, scope } => {
            let mut config = SignalConfig::load(args.rules_file.as_deref())?;
            let added = config.add_rule(SignalRule::new(SignalRuleKind::Website, scope, pattern)?);
            config.save(args.rules_file.as_deref())?;
            println!(
                "{} website rule in {}",
                if added { "added" } else { "already had" },
                path.display()
            );
            Ok(())
        }
        RulesCommand::RemoveApp { name, scope } => {
            let mut config = SignalConfig::load(args.rules_file.as_deref())?;
            let removed = config.remove_rule(SignalRuleKind::App, scope, &name);
            config.save(args.rules_file.as_deref())?;
            println!("removed {removed} app rule(s) from {}", path.display());
            Ok(())
        }
        RulesCommand::RemoveSite { pattern, scope } => {
            let mut config = SignalConfig::load(args.rules_file.as_deref())?;
            let removed = config.remove_rule(SignalRuleKind::Website, scope, &pattern);
            config.save(args.rules_file.as_deref())?;
            println!("removed {removed} website rule(s) from {}", path.display());
            Ok(())
        }
    }
}

fn request_from_line(line: &str, defaults: &CommonArgs) -> Result<StimRequest> {
    let mut parts = line.split_whitespace();
    let stim = <Stimulus as FromStr>::from_str(
        parts
            .next()
            .ok_or_else(|| anyhow!("line must start with vibe, beep, or zap"))?,
    )?;
    let query: HashMap<String, String> = parts
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    Ok(request_from_query(stim, &query, defaults))
}

fn request_from_query(
    stim: Stimulus,
    query: &HashMap<String, String>,
    defaults: &CommonArgs,
) -> StimRequest {
    StimRequest {
        stim,
        intensity: get_u8(query, "intensity", defaults.intensity, 1, 100),
        count: get_u8(query, "count", defaults.count, 1, 7),
        on_ms: get_u8(query, "on_ms", defaults.on_ms, 1, 255),
        off_ms: get_u8(query, "off_ms", defaults.off_ms, 1, 255),
        mode: query
            .get("mode")
            .and_then(|mode| match mode.as_str() {
                "response" => Some(WriteMode::Response),
                "no-response" => Some(WriteMode::NoResponse),
                _ => None,
            })
            .unwrap_or(defaults.mode),
    }
}

fn request_from_cloud_body(
    body: &str,
    defaults: &CommonArgs,
) -> std::result::Result<StimRequest, String> {
    let parsed: CloudStimulusBody =
        serde_json::from_str(body).map_err(|error| format!("invalid JSON body: {error}"))?;
    let stim = <Stimulus as FromStr>::from_str(parsed.stimulus.stimulus_type.as_str())
        .map_err(|_| "stimulus.stimulusType must be one of zap, beep, vibe".to_string())?;
    if !(1..=100).contains(&parsed.stimulus.stimulus_value) {
        return Err("stimulus.stimulusValue must be in range 1-100 inclusive".to_string());
    }
    Ok(StimRequest {
        stim,
        intensity: parsed.stimulus.stimulus_value as u8,
        count: 1,
        on_ms: defaults.on_ms.max(1),
        off_ms: defaults.off_ms.max(1),
        mode: defaults.mode,
    })
}

fn webtool_command_from_route(
    path: &str,
    query: &HashMap<String, String>,
    defaults: &CommonArgs,
) -> Result<WebtoolCommand> {
    let route = path.trim_start_matches("/webtool/");
    let mode = query
        .get("mode")
        .and_then(|mode| match mode.as_str() {
            "response" => Some(WriteMode::Response),
            "no-response" => Some(WriteMode::NoResponse),
            _ => None,
        })
        .unwrap_or(defaults.mode);
    match route {
        "testVibe" | "test-vibe" | "vibe" => Ok(WebtoolCommand {
            name: "testVibe",
            writes: vec![WebtoolWrite {
                characteristic: StimulusKey::Vibe,
                payload: vec![
                    0x80 | get_u8(query, "count", 1, 1, 7),
                    0x02,
                    get_u8(query, "intensity", 50, 1, 100),
                    get_u8(query, "on_ms", 22, 1, 255),
                    get_u8(query, "off_ms", 22, 1, 255),
                ],
            }],
            requires_zap: false,
            mode,
        }),
        "testBeep" | "test-beep" | "beep" => Ok(WebtoolCommand {
            name: "testBeep",
            writes: vec![WebtoolWrite {
                characteristic: StimulusKey::Beep,
                payload: vec![
                    0x80 | get_u8(query, "count", 1, 1, 7),
                    0x00,
                    get_u8(query, "intensity", 50, 1, 100),
                    get_u8(query, "on_ms", 22, 1, 255),
                    get_u8(query, "off_ms", 22, 1, 255),
                ],
            }],
            requires_zap: false,
            mode,
        }),
        "diagZap" | "diag-zap" | "zap" => {
            let slot = get_usize(query, "slot", 0, 0, 2);
            let zap_type = get_u8(query, "zap_type", 3, 0, 3);
            let mut writes = Vec::new();
            if zap_type == 1 || zap_type == 2 {
                let test_low = get_u16(query, "test_low", [10, 10, 10][slot], 0, u16::MAX);
                writes.push(WebtoolWrite {
                    characteristic: StimulusKey::Setup,
                    payload: vec![18, 85, 0, (test_low & 0xff) as u8, (test_low >> 8) as u8],
                });
            } else if zap_type == 3 {
                let count = get_u16(query, "zt_count", [1, 100, 50][slot], 0, u16::MAX);
                let release = get_u32(query, "zt_release", [1, 1, 3][slot]);
                let pause = get_u32(query, "zt_pause", [999, 99, 197][slot]);
                let mut payload = vec![18, 85, 5];
                payload.extend_from_slice(&count.to_le_bytes());
                payload.extend_from_slice(&release.to_le_bytes());
                payload.extend_from_slice(&pause.to_le_bytes());
                writes.push(WebtoolWrite {
                    characteristic: StimulusKey::Setup,
                    payload,
                });
            }
            writes.push(WebtoolWrite {
                characteristic: StimulusKey::Zap,
                payload: vec![
                    0x89 | (zap_type << 4),
                    get_u8(query, "level", [10, 10, 50][slot], 1, 100),
                ],
            });
            Ok(WebtoolCommand {
                name: "diagZap",
                writes,
                requires_zap: true,
                mode,
            })
        }
        "testLeds" | "test-leds" | "leds" => Ok(WebtoolCommand {
            name: "testLeds",
            writes: vec![WebtoolWrite {
                characteristic: StimulusKey::Leds,
                payload: vec![0x9f, 0xff, 0xfa, 0xfa],
            }],
            requires_zap: false,
            mode,
        }),
        "findPavlok" | "find-pavlok" => Ok(WebtoolCommand {
            name: "findPavlok",
            writes: vec![WebtoolWrite {
                characteristic: StimulusKey::Setup,
                payload: vec![19, 1, 2],
            }],
            requires_zap: false,
            mode,
        }),
        "findCancel" | "find-cancel" => Ok(WebtoolCommand {
            name: "findCancel",
            writes: vec![WebtoolWrite {
                characteristic: StimulusKey::Setup,
                payload: vec![19, 1, 0],
            }],
            requires_zap: false,
            mode,
        }),
        _ => bail!("unsupported Webtool command: {route}"),
    }
}

fn default_request(args: &CommonArgs, stim: Stimulus) -> StimRequest {
    StimRequest {
        stim,
        intensity: args.intensity.clamp(1, 100),
        count: args.count.clamp(1, 7),
        on_ms: args.on_ms.max(1),
        off_ms: args.off_ms.max(1),
        mode: args.mode,
    }
}

fn ensure_zap_allowed(stim: Stimulus, args: &CommonArgs) -> Result<()> {
    if stim == Stimulus::Zap && !args.allow_zap {
        bail!("zap requires --allow-zap");
    }
    Ok(())
}

fn validate_zap_config(args: &CommonArgs) -> Result<()> {
    if args.allow_zap && args.name.is_none() && args.uuid.is_none() {
        bail!("--allow-zap requires an explicit --name or --uuid target");
    }
    Ok(())
}

fn payload_for(request: &StimRequest) -> Vec<u8> {
    match request.stim {
        Stimulus::Vibe => vec![
            0x80 | request.count.clamp(1, 7),
            0x02,
            request.intensity.clamp(1, 100),
            request.on_ms.max(1),
            request.off_ms.max(1),
        ],
        Stimulus::Beep => vec![
            0x80 | request.count.clamp(1, 7),
            0x00,
            request.intensity.clamp(1, 100),
            request.on_ms.max(1),
            request.off_ms.max(1),
        ],
        Stimulus::Zap => vec![0x89, request.intensity.clamp(1, 100)],
    }
}

struct LocalDateParts {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    weekday: u8,
    month: u8,
    year: u8,
    tz_quarter_hours: i8,
}

struct AlarmTime {
    hour: u8,
    minute: u8,
}

fn local_date_parts(minute_offset: u16) -> Result<LocalDateParts> {
    let mut command = std::process::Command::new("/bin/date");
    if minute_offset > 0 {
        command.arg(format!("-v+{minute_offset}M"));
    }
    let output = command
        .arg("+%S %M %H %d %w %m %y %z")
        .output()
        .context("run /bin/date")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts = stdout.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 8 {
        bail!("unexpected /bin/date output: {stdout:?}");
    }
    let tz = parts[7];
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let tz_hours = tz
        .get(1..3)
        .ok_or_else(|| anyhow!("invalid timezone offset: {tz}"))?
        .parse::<i8>()
        .with_context(|| format!("parse timezone hours from {tz}"))?;
    let tz_minutes = tz
        .get(3..5)
        .ok_or_else(|| anyhow!("invalid timezone offset: {tz}"))?
        .parse::<i8>()
        .with_context(|| format!("parse timezone minutes from {tz}"))?;
    Ok(LocalDateParts {
        second: parse_date_u8(parts[0], "second")?,
        minute: parse_date_u8(parts[1], "minute")?,
        hour: parse_date_u8(parts[2], "hour")?,
        day: parse_date_u8(parts[3], "day")?,
        weekday: parse_date_u8(parts[4], "weekday")?,
        month: parse_date_u8(parts[5], "month")?,
        year: parse_date_u8(parts[6], "year")?,
        tz_quarter_hours: sign * (tz_hours * 4 + tz_minutes / 15),
    })
}

fn local_alarm_time(minute_offset: u16) -> Result<AlarmTime> {
    let parts = local_date_parts(minute_offset)?;
    Ok(AlarmTime {
        hour: parts.hour,
        minute: parts.minute,
    })
}

fn parse_date_u8(value: &str, label: &str) -> Result<u8> {
    value
        .parse::<u8>()
        .with_context(|| format!("parse {label} from {value:?}"))
}

fn pavlok_time_payload(parts: &LocalDateParts) -> Vec<u8> {
    vec![
        decimal_to_bcd(parts.second),
        decimal_to_bcd(parts.minute),
        decimal_to_bcd(parts.hour),
        decimal_to_bcd(parts.day),
        decimal_to_bcd(parts.weekday),
        decimal_to_bcd(parts.month),
        decimal_to_bcd(parts.year),
        parts.tz_quarter_hours as u8,
    ]
}

fn alarm_id_from_time(parts: &LocalDateParts) -> u16 {
    let seed = parts.hour as u16 * 60 + parts.minute as u16 + parts.second as u16;
    seed % 4095 + 1
}

fn wakeup_alarm_entries(
    start_minutes: u16,
    entries: u8,
    intensity: u8,
    name_prefix: &str,
    active: bool,
) -> Result<Vec<AlarmEntry>> {
    (0..entries)
        .map(|index| {
            let alarm_time = local_alarm_time(start_minutes + index as u16)
                .with_context(|| format!("read wake-up alarm time #{index}"))?;
            Ok(AlarmEntry {
                name: format!("{name_prefix} {}", index + 1),
                stim: Stimulus::Zap,
                hour: alarm_time.hour,
                minute: alarm_time.minute,
                alarm_id: WAKEUP_ALARM_ID_BASE + index as u16,
                intensity,
                count: index + 1,
                active,
            })
        })
        .collect()
}

fn build_alarm_file(profile: &str, entries: &[AlarmEntry]) -> Vec<u8> {
    let mut body_parts = Vec::with_capacity(1 + entries.len());
    body_parts.push(build_tlv("AP", profile.as_bytes()));
    for entry in entries {
        body_parts.push(build_tlv("HA", &build_alarm_entry(entry)));
    }
    let body = concat_bytes(&body_parts);
    let mut payload = Vec::with_capacity(6 + body.len());
    payload.extend_from_slice(b"AH");
    payload.extend_from_slice(&((body.len() + 2) as u16).to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&body);
    let crc = crc_ccitt(&payload, 0xffff);
    payload[4..6].copy_from_slice(&crc.to_le_bytes());
    payload
}

fn build_alarm_entry(entry: &AlarmEntry) -> Vec<u8> {
    concat_bytes(&[
        build_tlv("AN", entry.name.as_bytes()),
        build_tlv(
            "TM",
            &[
                0,
                decimal_to_bcd(entry.minute),
                decimal_to_bcd(entry.hour),
                if entry.active { 0x80 } else { 0 },
            ],
        ),
        build_tlv("WD", &[1]),
        build_tlv("WI", &120u16.to_le_bytes()),
        build_tlv("SN", &[0]),
        build_tlv("AO", &[u8::from(entry.active)]),
        build_tlv("ID", &(entry.alarm_id % 4096).to_le_bytes()),
        build_alarm_stim(entry.stim, entry.intensity, entry.count),
    ])
}

fn build_alarm_stim(stim: Stimulus, intensity: u8, count: u8) -> Vec<u8> {
    match stim {
        Stimulus::Vibe => build_vibe_stim(intensity, count),
        Stimulus::Beep => build_beep_stim(intensity, count),
        Stimulus::Zap => build_zap_stim(intensity, count),
    }
}

fn build_vibe_stim(intensity: u8, count: u8) -> Vec<u8> {
    let motor_command = [
        b'M',
        b'C',
        5,
        0,
        0x80 | count.clamp(1, 7),
        12,
        intensity.clamp(1, 100),
        250,
        250,
    ];
    build_tlv("MH", &motor_command)
}

fn build_beep_stim(intensity: u8, count: u8) -> Vec<u8> {
    let beep_command = [
        b'P',
        b'C',
        5,
        0,
        0x80 | count.clamp(1, 7),
        0,
        intensity.clamp(1, 100),
        250,
        250,
    ];
    build_tlv("PH", &beep_command)
}

fn build_zap_stim(intensity: u8, count: u8) -> Vec<u8> {
    let zap_command = [
        b'Z',
        b'C',
        2,
        0,
        0x80 | count.clamp(1, 7),
        intensity.clamp(1, 100),
    ];
    build_tlv("ZH", &zap_command)
}

fn build_tlv(tag: &str, value: &[u8]) -> Vec<u8> {
    let bytes = tag.as_bytes();
    let mut output = Vec::with_capacity(4 + value.len());
    output.extend_from_slice(&bytes[..2]);
    output.extend_from_slice(&(value.len() as u16).to_le_bytes());
    output.extend_from_slice(value);
    output
}

fn concat_bytes(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for chunk in chunks {
        output.extend_from_slice(chunk);
    }
    output
}

fn decimal_to_bcd(value: u8) -> u8 {
    (value / 10) << 4 | (value % 10)
}

fn bcd_to_decimal(value: u8) -> u8 {
    10 * (value >> 4) + (value & 0x0f)
}

fn crc_ccitt(bytes: &[u8], initial: u16) -> u16 {
    let mut crc = initial;
    for byte in bytes {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn parse_next_alarm(payload: &[u8]) -> Option<String> {
    if payload.len() < 7 {
        return None;
    }
    let minute = bcd_to_decimal(payload[1]);
    let hour = bcd_to_decimal(payload[2]);
    let weekday = payload[4] as usize;
    let id = u16::from_le_bytes([payload[5], payload[6]]);
    if id == 0 {
        return Some("none".to_string());
    }
    let day = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
        .get(weekday)
        .copied()
        .unwrap_or("?");
    Some(format!("{day} {hour:02}:{minute:02} (id #{id})"))
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let query = raw_query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    (path.to_string(), query)
}

fn get_u8(query: &HashMap<String, String>, key: &str, default: u8, min: u8, max: u8) -> u8 {
    query
        .get(key)
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn get_u16(query: &HashMap<String, String>, key: &str, default: u16, min: u16, max: u16) -> u16 {
    query
        .get(key)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn get_u32(query: &HashMap<String, String>, key: &str, default: u32) -> u32 {
    query
        .get(key)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
}

fn decode_query_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'+' {
            output.push(b' ');
            index += 1;
        } else if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            output.push(byte);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn get_usize(
    query: &HashMap<String, String>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> usize {
    query
        .get(key)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn sample_latency_ms(timing: &Timing) -> f64 {
    timing.trigger_to_write_ack.unwrap_or_else(|| {
        timing.trigger_to_write_issued.unwrap_or(0.0) + timing.write_call_wall.unwrap_or(0.0)
    })
}

fn merge_write_timing(target: &mut Timing, source: Timing) {
    if target.trigger_to_write_issued.is_none() {
        target.trigger_to_write_issued = source.trigger_to_write_issued;
    }
    target.write_call_wall =
        Some(target.write_call_wall.unwrap_or(0.0) + source.write_call_wall.unwrap_or(0.0));
    target.trigger_to_write_ack = source.trigger_to_write_ack.or(target.trigger_to_write_ack);
}

fn matches_target(peripheral: &Peripheral, name: &str, args: &CommonArgs) -> bool {
    if matches_uuid(peripheral, args) {
        return true;
    }
    if let Some(target_name) = &args.name {
        return name == target_name
            || name
                .to_ascii_lowercase()
                .contains(&target_name.to_ascii_lowercase());
    }
    is_pavlok_name(name)
}

fn matches_uuid(peripheral: &Peripheral, args: &CommonArgs) -> bool {
    args.uuid
        .as_ref()
        .is_some_and(|target| peripheral.id().to_string().eq_ignore_ascii_case(target))
}

fn is_pavlok_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("pavlok")
}

fn timing_with_response(started: Instant) -> Timing {
    Timing {
        request_to_response: Some(ms_since(started)),
        ..Timing::default()
    }
}

fn ms_since(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn ms_between(start: Instant, end: Instant) -> f64 {
    end.duration_since(start).as_secs_f64() * 1_000.0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sample_summary(values: &[f64]) -> String {
    if values.is_empty() {
        return "n=0".to_string();
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    format!(
        "n={} mean={:.3}ms median={:.3}ms min={:.3}ms max={:.3}ms",
        values.len(),
        mean,
        sorted[values.len() / 2],
        sorted.first().copied().unwrap_or_default(),
        sorted.last().copied().unwrap_or_default()
    )
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(value).context("serialize JSON")?
    );
    Ok(())
}

fn setup_logging(path: Option<&str>) -> Result<()> {
    if let Some(path) = path {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open log file {path}"))?;
        let stderr = file.try_clone().context("clone log file")?;
        unsafe {
            let stdout_fd = std::os::fd::AsRawFd::as_raw_fd(&file);
            let stderr_fd = std::os::fd::AsRawFd::as_raw_fd(&stderr);
            libc_dup2(stdout_fd, 1)?;
            libc_dup2(stderr_fd, 2)?;
        }
        std::io::stdout().flush().ok();
        std::io::stderr().flush().ok();
    }
    Ok(())
}

unsafe fn libc_dup2(from: i32, to: i32) -> Result<()> {
    unsafe extern "C" {
        fn dup2(from: i32, to: i32) -> i32;
    }
    if unsafe { dup2(from, to) } == -1 {
        bail!("dup2 failed")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn violation_zap_counts_reset_at_new_5am_window() {
        let mut counters = ViolationZapCounters {
            path: None,
            state: ViolationZapCounterState::default(),
        };

        assert_eq!(
            counters.next_count_for_window("app:dnd:messages", "2026-05-12"),
            1
        );
        assert_eq!(
            counters.next_count_for_window("app:dnd:messages", "2026-05-12"),
            2
        );
        assert_eq!(
            counters.next_count_for_window("app:dnd:outlook", "2026-05-12"),
            1
        );
        assert_eq!(
            counters.next_count_for_window("app:dnd:messages", "2026-05-13"),
            1
        );
    }

    #[test]
    fn zap_alarm_stimulus_encodes_count_and_interval() {
        let payload = build_zap_stim(50, 2);
        assert_eq!(payload, vec![b'Z', b'H', 6, 0, b'Z', b'C', 2, 0, 0x82, 50]);
    }

    #[test]
    fn inactive_alarm_clears_time_active_bit() {
        let entry = AlarmEntry {
            name: "Wake".to_string(),
            stim: Stimulus::Zap,
            hour: 9,
            minute: 5,
            alarm_id: 905,
            intensity: 50,
            count: 1,
            active: false,
        };

        let payload = build_alarm_entry(&entry);
        let time = build_tlv("TM", &[0, decimal_to_bcd(5), decimal_to_bcd(9), 0]);
        assert!(payload.windows(time.len()).any(|window| window == time));
    }

    #[test]
    fn payloads_match_protocol_bytes() {
        let request = StimRequest {
            stim: Stimulus::Vibe,
            intensity: 50,
            count: 1,
            on_ms: 22,
            off_ms: 22,
            mode: WriteMode::Response,
        };

        assert_eq!(payload_for(&request), vec![0x81, 0x02, 50, 22, 22]);
        assert_eq!(
            payload_for(&StimRequest {
                stim: Stimulus::Beep,
                ..request
            }),
            vec![0x81, 0x00, 50, 22, 22]
        );
        assert_eq!(
            payload_for(&StimRequest {
                stim: Stimulus::Zap,
                ..request
            }),
            vec![0x89, 50]
        );
    }

    #[test]
    fn payload_values_are_clamped() {
        let request = StimRequest {
            stim: Stimulus::Vibe,
            intensity: 200,
            count: 99,
            on_ms: 0,
            off_ms: 0,
            mode: WriteMode::Response,
        };

        assert_eq!(payload_for(&request), vec![0x87, 0x02, 100, 1, 1]);
    }

    #[test]
    fn parses_http_target_query() {
        let (path, query) = parse_target("/stim/vibe?intensity=55&mode=no-response");

        assert_eq!(path, "/stim/vibe");
        assert_eq!(query.get("intensity").map(String::as_str), Some("55"));
        assert_eq!(query.get("mode").map(String::as_str), Some("no-response"));
    }

    #[test]
    fn parses_monitor_zap_url_and_appends_defaults() {
        let parsed = parse_http_url("http://127.0.0.1:8765/stim/zap").expect("parse url");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8765);
        assert_eq!(parsed.target, "/stim/zap");

        let target = target_with_stim_defaults(&parsed.target, &test_defaults());
        assert_eq!(
            target,
            "/stim/zap?intensity=50&count=1&on_ms=22&off_ms=22&mode=response"
        );

        let target = target_with_stim_defaults("/stim/zap?source=monitor", &test_defaults());
        assert_eq!(
            target,
            "/stim/zap?source=monitor&intensity=50&count=1&on_ms=22&off_ms=22&mode=response"
        );
    }

    #[test]
    fn parses_http_head_only_after_headers_are_complete() {
        assert!(
            parse_http_head(b"POST /api/v5/stimulus/send HTTP/1.1\r\nContent-Length: 4\r\n")
                .expect("partial header")
                .is_none()
        );

        let request =
            b"POST /api/v5/stimulus/send HTTP/1.1\r\nHost: local\r\nContent-Length: 4\r\n\r\nbody";
        let head = parse_http_head(request)
            .expect("parse head")
            .expect("complete head");

        assert_eq!(head.method, "POST");
        assert_eq!(head.target, "/api/v5/stimulus/send");
        assert_eq!(head.content_length, 4);
        assert_eq!(
            &request[head.body_start..head.body_start + head.content_length],
            b"body"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reads_http_body_after_split_packets() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            read_http_request(&mut stream).await.expect("read request")
        });

        let mut client = TcpStream::connect(addr).await.expect("connect test client");
        client
            .write_all(b"POST /api/v5/stimulus/send HTTP/1.1\r\nContent-Length: 4\r\n\r\nbo")
            .await
            .expect("write first chunk");
        sleep(Duration::from_millis(10)).await;
        client.write_all(b"dy").await.expect("write second chunk");

        let request = server.await.expect("server task");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/api/v5/stimulus/send");
        assert_eq!(request.body, "body");
    }

    #[test]
    fn cloud_stimulus_json_maps_to_local_stimulus_request() {
        let defaults = test_defaults();
        let request = request_from_cloud_body(
            r#"{"stimulus":{"stimulusType":"beep","stimulusValue":42}}"#,
            &defaults,
        )
        .expect("valid cloud stimulus");

        assert_eq!(request.stim, Stimulus::Beep);
        assert_eq!(request.intensity, 42);
        assert_eq!(request.count, 1);
        assert_eq!(request.on_ms, 22);
        assert_eq!(request.off_ms, 22);
        assert_eq!(request.mode, WriteMode::Response);
    }

    #[test]
    fn cloud_stimulus_json_rejects_invalid_type_and_value() {
        let defaults = test_defaults();

        assert!(
            request_from_cloud_body(
                r#"{"stimulus":{"stimulusType":"led","stimulusValue":42}}"#,
                &defaults
            )
            .is_err()
        );
        assert!(
            request_from_cloud_body(
                r#"{"stimulus":{"stimulusType":"vibe","stimulusValue":0}}"#,
                &defaults
            )
            .is_err()
        );
        assert!(
            request_from_cloud_body(
                r#"{"stimulus":{"stimulusType":"vibe","stimulusValue":101}}"#,
                &defaults
            )
            .is_err()
        );
    }

    #[test]
    fn webtool_routes_map_to_documented_packets() {
        let defaults = test_defaults();

        assert_eq!(
            webtool_writes(
                webtool_command_from_route("/webtool/testVibe", &HashMap::new(), &defaults)
                    .expect("test vibe")
            ),
            vec![(StimulusKey::Vibe, vec![0x81, 0x02, 50, 22, 22])]
        );
        assert_eq!(
            webtool_writes(
                webtool_command_from_route("/webtool/testBeep", &HashMap::new(), &defaults)
                    .expect("test beep")
            ),
            vec![(StimulusKey::Beep, vec![0x81, 0x00, 50, 22, 22])]
        );

        let diag_zap = webtool_command_from_route("/webtool/diagZap", &HashMap::new(), &defaults)
            .expect("diag zap");
        assert!(diag_zap.requires_zap);
        assert_eq!(
            webtool_writes(diag_zap),
            vec![
                (
                    StimulusKey::Setup,
                    vec![18, 85, 5, 1, 0, 1, 0, 0, 0, 231, 3, 0, 0]
                ),
                (StimulusKey::Zap, vec![0xb9, 10])
            ]
        );
        assert_eq!(
            webtool_writes(
                webtool_command_from_route(
                    "/webtool/diagZap",
                    &query("zap_type=0&level=50"),
                    &defaults,
                )
                .expect("simple diag zap")
            ),
            vec![(StimulusKey::Zap, vec![0x89, 50])]
        );
        assert_eq!(
            webtool_writes(
                webtool_command_from_route("/webtool/testLeds", &HashMap::new(), &defaults)
                    .expect("test leds")
            ),
            vec![(StimulusKey::Leds, vec![0x9f, 0xff, 0xfa, 0xfa])]
        );
        assert_eq!(
            webtool_writes(
                webtool_command_from_route("/webtool/findPavlok", &HashMap::new(), &defaults)
                    .expect("find pavlok")
            ),
            vec![(StimulusKey::Setup, vec![19, 1, 2])]
        );
        assert_eq!(
            webtool_writes(
                webtool_command_from_route("/webtool/findCancel", &HashMap::new(), &defaults)
                    .expect("find cancel")
            ),
            vec![(StimulusKey::Setup, vec![19, 1, 0])]
        );
    }

    #[test]
    fn zap_requires_allow_zap_and_explicit_target() {
        let mut defaults = test_defaults();

        assert!(ensure_zap_allowed(Stimulus::Zap, &defaults).is_err());

        defaults.allow_zap = true;
        assert!(validate_zap_config(&defaults).is_err());

        defaults.name = Some("Pavlok-3-E14D".to_string());
        assert!(validate_zap_config(&defaults).is_ok());
        assert!(ensure_zap_allowed(Stimulus::Zap, &defaults).is_ok());

        defaults.name = None;
        defaults.uuid = Some("ed915cd5-de1c-844c-8941-cea7b83f4c0f".to_string());
        assert!(validate_zap_config(&defaults).is_ok());
    }

    #[test]
    fn no_response_sample_latency_includes_awaited_write_time() {
        let timing = Timing {
            trigger_to_write_issued: Some(2.0),
            write_call_wall: Some(7.5),
            trigger_to_write_ack: None,
            request_to_response: None,
        };

        assert_eq!(sample_latency_ms(&timing), 9.5);
    }

    #[test]
    fn screen_time_thresholds_start_at_twenty_minutes_then_every_five() {
        assert_eq!(screen_time_threshold_minutes(19 * 60 + 59), None);
        assert_eq!(screen_time_threshold_minutes(20 * 60), Some(20));
        assert_eq!(screen_time_threshold_minutes(24 * 60 + 59), Some(20));
        assert_eq!(screen_time_threshold_minutes(25 * 60), Some(25));
        assert_eq!(screen_time_threshold_minutes(30 * 60), Some(30));
    }

    #[test]
    fn youtube_screen_time_filter_matches_domains_and_urls() {
        let filter = youtube_screen_time_sql_filter();
        assert!(filter.contains("WEBDOMAIN"));
        assert!(filter.contains("youtube.com"));
        assert!(filter.contains("youtu.be"));
        assert!(filter.contains("WEBPAGEURL"));
    }

    #[test]
    fn decodes_url_encoded_alarm_names() {
        assert_eq!(decode_query_value("Codex%20demo"), "Codex demo");
        assert_eq!(decode_query_value("Outlook+limit"), "Outlook limit");
    }

    fn webtool_writes(command: WebtoolCommand) -> Vec<(StimulusKey, Vec<u8>)> {
        command
            .writes
            .into_iter()
            .map(|write| (write.characteristic, write.payload))
            .collect()
    }

    fn query(raw: &str) -> HashMap<String, String> {
        raw.split('&')
            .filter_map(|part| part.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn test_defaults() -> CommonArgs {
        CommonArgs {
            name: None,
            uuid: None,
            scan_timeout_ms: 8_000,
            connect_timeout_ms: 15_000,
            mode: WriteMode::Response,
            intensity: 50,
            count: 1,
            on_ms: 22,
            off_ms: 22,
            allow_zap: false,
            log_file: None,
        }
    }
}
