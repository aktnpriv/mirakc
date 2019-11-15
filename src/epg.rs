use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::iter::FromIterator;
use std::path::PathBuf;

use actix::prelude::*;
use chrono::{DateTime, Duration};
use humantime;
use indexmap::IndexMap;
use log;
use mustache;
use serde::{Deserialize, Serialize};

use crate::config::{Config, ChannelConfig};
use crate::datetime_ext::*;
use crate::error::Error;
use crate::messages::{OpenTunerBy, OpenTunerMessage, UpdateEpgMessage};
use crate::models::*;
use crate::resource_manager;
use crate::tuner::{TunerOutput, TunerUser};

pub fn start(arbiter: &Arbiter, config: &Config) {
    let epg = Epg::new(config);
    arbiter.exec_fn(|| { epg.start(); });
}

type EpgServices = Vec<EpgService>;

struct Epg {
    cache_dir: PathBuf,
    scan_services: String,
    collect_eits: String,
    channels: Vec<ChannelConfig>,
    schedules: HashMap<EpgScheduleId, EpgSchedule>,
    max_elapsed: Option<Duration>,
}

impl Epg {
    #[inline]
    fn scan_services_time_limit(channel_type: ChannelType) -> Duration {
        match channel_type {
            ChannelType::GR => Duration::seconds(10),
            ChannelType::BS => Duration::seconds(20),
            _ => Duration::minutes(30),
        }
    }

    #[inline]
    fn collect_eits_time_limit(channel_type: ChannelType) -> Duration {
        match channel_type {
            ChannelType::GR => Duration::minutes(1) + Duration::seconds(10),
            ChannelType::BS => Duration::minutes(6) + Duration::seconds(30),
            _ => Duration::minutes(10),
        }
    }

    #[inline]
    fn format_duration(duration: Duration) -> humantime::FormattedDuration {
        humantime::format_duration(duration.to_std().unwrap())
    }

    fn new(config: &Config) -> Self {
        let channels = config.channels
            .iter()
            .filter(|config| !config.disabled)
            .cloned()
            .collect();

        Epg {
            cache_dir: PathBuf::from(&config.epg_cache_dir),
            scan_services: config.tools.scan_services.clone(),
            collect_eits: config.tools.collect_eits.clone(),
            channels,
            schedules: HashMap::new(),
            max_elapsed: None,
        }
    }

    fn run_later(&mut self, ctx: &mut Context<Self>, duration: Duration) {
        log::info!("Run after {}", Self::format_duration(duration));
        ctx.run_later(duration.to_std().unwrap(), Self::run);
    }

    fn run(&mut self, ctx: &mut Context<Self>) {
        let now = Jst::now();

        let remaining = now.date().succ().and_hms(0, 0, 0) - now;
        if remaining < self.estimate_time() {
            log::info!("This task may not be completed this day");
            log::info!("Postpone the task until next day \
                        in order to keep consistency of EPG data");
            self.run_later(ctx, remaining + Duration::seconds(10));
            return;
        }

        actix::fut::ok::<_, Error, Self>(())
            .and_then(|_, epg, _ctx| {
                epg.scan_services()
            })
            .and_then(|services, epg, _ctx| {
                epg.update_schedules(services)
            })
            .and_then(|_, epg, _ctx| {
                match epg.save_epg_data() {
                    Ok(_) => actix::fut::ok(()),
                    Err(err) => actix::fut::err(err),
                }
            })
            .and_then(|_, epg, _ctx| {
                epg.send_update_epg_message();
                actix::fut::ok(())
            })
            .then(move |result, epg, ctx| {
                let elapsed = Jst::now() - now;
                let duration = match result {
                    Ok(_) => {
                        log::info!("Done, {} elapsed",
                                   Self::format_duration(elapsed));
                        epg.update_max_elapsed(elapsed);
                        Duration::minutes(15)
                    }
                    Err(err) => {
                        log::error!("Failed: {}", err);
                        Duration::minutes(5)
                    }
                };
                epg.run_later(ctx, duration);
                actix::fut::ok(())
            })
            .spawn(ctx);
    }

    fn estimate_time(&self) -> Duration {
        match self.max_elapsed {
            Some(max_elapsed) => max_elapsed + Duration::seconds(30),
            None => Duration::hours(1),
        }
    }

    fn update_max_elapsed(&mut self, elapsed: Duration) {
        let do_update = match self.max_elapsed {
            Some(max_elapsed) if elapsed <= max_elapsed => false,
            _ => true,
        };
        if do_update {
            log::info!("Updated the max elapsed time");
            self.max_elapsed = Some(elapsed);
        }
    }

    fn scan_services(
        &mut self,
    ) -> impl ActorFuture<Item = EpgServices, Error = Error, Actor = Self> {
        let channels = self.collect_channels_for_scanning_services();
        let stream = futures::stream::iter_ok::<_, Error>(channels);
        let stream = actix::fut::wrap_stream::<_, Self>(stream);

        stream
            .map(|ch, _epg, _ctx| {
                log::info!("Scanning services in {}...", ch.name);
                ch
            })
            .and_then(|ch, _epg, _ctx| {
                let msg = OpenTunerMessage {
                    by: ch.clone().into(),
                    user: TunerUser::background("epg".to_string()),
                    duration: Some(
                        Self::scan_services_time_limit(ch.channel_type)),
                    preprocess: false,
                    postprocess: false,
                };

                let req = resource_manager::open_tuner(msg)
                    .map(|output| (ch, output));

                actix::fut::wrap_future(req)
            })
            .and_then(|(ch, output), epg, _ctx| {
                let cmd = match Self::make_command(&epg.scan_services, &ch) {
                    Ok(cmd) => cmd,
                    Err(err) => return actix::fut::err(err),
                };
                match output.pipe(&cmd) {
                    Ok(output) => actix::fut::ok((ch, output)),
                    Err(err) => actix::fut::err(Error::from(err)),
                }
            })
            .and_then(|(ch, output), _epg, _ctx| {
                let reader = BufReader::new(output);
                match serde_json::from_reader::<_, Vec<TsService>>(reader) {
                    Ok(services) => {
                        log::info!("Found {} services in {}",
                                   services.len(), ch.name);
                        let mut epg_services = Vec::new();
                        for service in services.iter() {
                            epg_services.push(EpgService::from((&ch, service)));
                        }
                        actix::fut::ok(Some(epg_services))
                    }
                    Err(_) => {
                        log::warn!("No service.  Maybe, the broadcast service \
                                    has been suspended.");
                        actix::fut::ok(None)
                    }
                }
            })
            .fold(Vec::new(), |mut result, services, _epg, _ctx| {
                match services {
                    Some(mut services) => result.append(&mut services),
                    None => (),
                }
                actix::fut::ok::<_, Error, Self>(result)
            })
    }

    fn collect_channels_for_scanning_services(&self) -> Vec<EpgChannel> {
        self.channels
            .iter()
            .cloned()
            .map(EpgChannel::from)
            .collect()
    }

    fn update_schedules(
        &mut self,
        services: EpgServices,
    ) -> impl ActorFuture<Item = (), Error = Error, Actor = Self> {
        self.prepare_schedules(&services, Jst::now());

        let channels = self.collect_channels_for_collecting_programs(&services);
        let stream = futures::stream::iter_ok::<_, Error>(channels);
        let stream = actix::fut::wrap_stream::<_, Self>(stream);

        stream
            .map(|(nid, ch), _epg, _ctx| {
                log::info!("Updating schedule of {}...", ch.name);
                (nid, ch)
            })
            .and_then(|(nid, ch), _epg, _ctx| {
                let msg = OpenTunerMessage {
                    by: ch.clone().into(),
                    user: TunerUser::background("epg".to_string()),
                    duration: Some(
                        Self::collect_eits_time_limit(ch.channel_type)),
                    preprocess: false,
                    postprocess: false,
                };

                let req = resource_manager::open_tuner(msg)
                    .map(move |output| (nid, ch, output));

                actix::fut::wrap_future(req)
            })
            .and_then(|(_nid, ch, output), epg, _ctx| {
                let cmd = match Self::make_command(&epg.collect_eits, &ch) {
                    Ok(cmd) => cmd,
                    Err(err) => return actix::fut::err(err),
                };
                match output.pipe(&cmd) {
                    Ok(output) => actix::fut::ok(output),
                    Err(err) => actix::fut::err(Error::from(err)),
                }
            })
            .and_then(|output, epg, _ctx| {
                match epg.update_tables(output) {
                    Ok(_) => actix::fut::ok(()),
                    Err(err) => actix::fut::err(err),
                }
            })
            .finish()
    }

    fn prepare_schedules(
        &mut self, services: &[EpgService], timestamp: DateTime<Jst>) {
        let mut unused_ids: HashSet<_> =
            HashSet::from_iter(self.schedules.keys().cloned());

        let midnight = timestamp.date().and_hms(0, 0, 0);

        for service in services {
            let id = EpgScheduleId::from(
                (service.nid, service.tsid, service.sid));
            self.schedules
                .entry(id)
                .and_modify(|sched| {
                    if sched.updated_at < midnight {
                        // Save overnight events.  The overnight events will be
                        // lost in `update_tables()`.
                        sched.save_overnight_events(midnight);
                    }
                    sched.updated_at = timestamp;
                })
                .or_insert(EpgSchedule::new(&service));
            unused_ids.remove(&id);
        }

        // Removing "garbage" schedules.
        for id in unused_ids.iter() {
            self.schedules.remove(&id);
            log::debug!("Removed schedule#{}", id);
        }
    }

    fn collect_channels_for_collecting_programs(
        &self,
        services: &[EpgService],
    ) -> HashMap<u16, EpgChannel> {
        let mut map: HashMap<u16, EpgChannel> = HashMap::new();
        for sv in services.iter() {
            map.entry(sv.nid).and_modify(|ch| {
                ch.excluded_services.extend(&sv.channel.excluded_services);
            }).or_insert(sv.channel.clone());
        }
        map
    }

    fn update_tables(&mut self, output: TunerOutput) -> Result<(), Error> {
        // TODO: use async/await
        let mut reader = BufReader::new(output);
        let mut json = String::new();
        let mut num_sections = 0;
        while reader.read_line(&mut json)? > 0 {
            let section = serde_json::from_str::<EitSection>(&json)?;
            let sched_id = section.epg_schedule_id();
            self.schedules.entry(sched_id).and_modify(|sched| {
                sched.update(section);
            });
            json.clear();
            num_sections += 1;
        }
        log::debug!("Collected {} EIT sections", num_sections);
        return Ok(());
    }

    fn load_epg_data(&mut self) -> Result<(), Error> {
        self.load_schedules()?;
        Ok(())
    }

    fn load_schedules(&mut self) -> Result<(), Error> {
        let json_path = self.cache_dir.join("schedules.json");
        log::debug!("Loading schedules from {}...", json_path.display());
        let reader = BufReader::new(File::open(&json_path)?);
        self.schedules = serde_json::from_reader(reader)?;
        log::info!("Loaded schedules from {}...", json_path.display());
        Ok(())
    }

    fn save_epg_data(&self) -> Result<(), Error> {
        self.save_schedules()?;
        Ok(())
    }

    fn save_schedules(&self) -> Result<(), Error> {
        let json_path = self.cache_dir.join("schedules.json");
        log::debug!("Saving schedules into {}...", json_path.display());
        let writer = BufWriter::new(File::create(&json_path)?);
        serde_json::to_writer(writer, &self.schedules)?;
        log::info!("Saved schedules into {}...", json_path.display());
        Ok(())
    }

    fn send_update_epg_message(&self) {
        let msg = UpdateEpgMessage {
            services: self.collect_epg_services(),
            programs: self.collect_epg_programs(),
        };
        resource_manager::update_epg(msg);
    }

    fn collect_epg_services(&self) -> Vec<ServiceModel> {
        let mut services: Vec<ServiceModel> = Vec::new();
        for sched in self.schedules.values() {
            sched.collect_epg_service(&mut services);
        }
        log::info!("Collected {} services", services.len());
        services
    }

    fn collect_epg_programs(&self) -> HashMap<u64, ProgramModel> {
        let mut programs: HashMap<u64, ProgramModel> = HashMap::new();
        for (&sched_id, sched) in self.schedules.iter() {
            sched.collect_epg_programs(sched_id, &mut programs);
        }
        log::info!("Collected {} programs", programs.len());
        programs
    }

    fn make_command(src: &str, channel: &EpgChannel) -> Result<String, Error> {
        let template = mustache::compile_str(src)?;
        let data = mustache::MapBuilder::new()
            .insert("xsids", &channel.excluded_services)?.build();
        Ok(template.render_data_to_string(&data)?)
    }
}

impl Actor for Epg {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        log::info!("Started");
        match self.load_epg_data() {
            Ok(_) => self.send_update_epg_message(),
            Err(err) => log::error!("Failed to load EPG data: {}", err),
        }
        ctx.run_later(Duration::minutes(0).to_std().unwrap(), Self::run);
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        log::info!("Stopped");
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[derive(Deserialize, Serialize)]
struct EpgScheduleId(u64);

impl EpgScheduleId {
    #[inline]
    fn nid(&self) -> u16 {
        ((self.0 >> 32) & 0xFFFF) as u16
    }

    #[allow(dead_code)]
    #[inline]
    fn tsid(&self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }

    #[inline]
    fn sid(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }
}

impl fmt::Display for EpgScheduleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:012X}", self.0)
    }
}

impl From<(u16, u16, u16)> for EpgScheduleId {
    #[inline]
    fn from(triple: (u16, u16, u16)) -> Self {
        EpgScheduleId(
            (triple.0 as u64) << 32 |
            (triple.1 as u64) << 16 |
            (triple.2 as u64)       )
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EpgSchedule {
    service: EpgService,
    // In Japan, only the following indexes are used:
    //
    //    0 | 8 | 16 | 24 => the former 4 days of 8 days schedule
    //    1 | 9 | 17 | 25 => the later 4 days of 8 days schedule
    tables: [Option<Box<EpgTable>>; 32],
    overnight_events: Vec<EitEvent>,
    #[serde(with = "serde_jst")]
    updated_at: DateTime<Jst>,
}

impl EpgSchedule {
    fn new(service: &EpgService) -> EpgSchedule {
        EpgSchedule {
            service: service.clone(),
            tables: Default::default(),
            overnight_events: Vec::new(),
            updated_at: Jst::now(),
        }
    }

    fn update(&mut self, section: EitSection) {
        let i = section.table_index();
        if self.tables[i].is_none() {
            self.tables[i] = Some(Box::new(EpgTable::default()));
        }
        self.tables[i].as_mut().unwrap().update(section);
    }

    fn save_overnight_events(&mut self, midnight: DateTime<Jst>) {
        let mut events = Vec::new();
        for table in self.tables.iter() {
            if let Some(ref table) = table {
                events = table.collect_overnight_events(midnight, events);
            }
        }
        log::debug!("Saved {} overnight events of schedule#{}",
                    events.len(), self.service.schedule_id());
        self.overnight_events = events;
    }

    fn collect_epg_service(&self, services: &mut Vec<ServiceModel>) {
        services.push(self.service.clone().into());
    }

    fn collect_epg_programs(
        &self,
        sched_id: EpgScheduleId,
        programs: &mut HashMap<u64, ProgramModel>) {
        let sid = sched_id.sid();
        let nid = sched_id.nid();
        for event in self.overnight_events.iter() {
            let eid = event.event_id;
            programs
                .entry(ProgramModel::make_id(eid, sid, nid))
                .or_insert(ProgramModel::new(eid, sid, nid))
                .update(event);
        }
        for table in self.tables.iter() {
            if let Some(table) = table {
                table.collect_epg_programs(sched_id, programs)
            }
        }
    }
}

#[derive(Default)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
// A table contains TV program information about 4 days of a TV program
// schedule.
struct EpgTable {
    // Segments are stored in chronological order.
    //
    // The first 8 consecutive segments contains TV program information for the
    // first day.
    segments: [EpgSegment; 32],
}

impl EpgTable {
    fn update(&mut self, section: EitSection) {
        let i = section.segment_index();
        self.segments[i].update(section);
    }

    fn collect_overnight_events(
        &self,
        midnight: DateTime<Jst>,
        mut events: Vec<EitEvent>
    ) -> Vec<EitEvent> {
        for segment in self.segments.iter() {
            events = segment.collect_overnight_events(midnight, events);
        }
        events
    }

    fn collect_epg_programs(
        &self,
        sched_id: EpgScheduleId,
        programs: &mut HashMap<u64, ProgramModel>) {
        for segment in self.segments.iter() {
            segment.collect_epg_programs(sched_id, programs)
        }
    }
}

#[derive(Default)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
// A segment contains TV program information about 3 hours of a TV program
// schedule.
struct EpgSegment {
    // Sections are stored in chronological order.
    sections: [Option<EpgSection>; 8],
}

impl EpgSegment {
    fn update(&mut self, section: EitSection) {
        let n = section.last_section_index() + 1;
        for i in n..8 {
            self.sections[i] = None;
        }

        let i = section.section_index();
        self.sections[i] = Some(EpgSection::from(section));
    }

    fn collect_overnight_events(
        &self,
        midnight: DateTime<Jst>,
        events: Vec<EitEvent>
    ) -> Vec<EitEvent> {
        self.sections
            .iter()
            .filter(|section| section.is_some())
            .map(|section| section.as_ref().unwrap())
            .fold(events, |events_, section| {
                section.collect_overnight_events(midnight, events_)
            })
    }

    fn collect_epg_programs(
        &self,
        sched_id: EpgScheduleId,
        programs: &mut HashMap<u64, ProgramModel>) {
        for section in self.sections.iter() {
            if let Some(section) = section {
                section.collect_epg_programs(sched_id, programs)
            }
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EpgSection {
    version: u8,
    // Events are stored in chronological order.
    events: Vec<EitEvent>,
}

impl EpgSection {
    fn collect_overnight_events(
        &self,
        midnight: DateTime<Jst>,
        mut events: Vec<EitEvent>
    ) -> Vec<EitEvent> {
        for event in self.events.iter() {
            if event.is_overnight_event(midnight) {
                events.push(event.clone());
            }
        }
        events
    }

    fn collect_epg_programs(
        &self,
        sched_id: EpgScheduleId,
        programs: &mut HashMap<u64, ProgramModel>) {
        let sid = sched_id.sid();
        let nid = sched_id.nid();
        for event in self.events.iter() {
            let eid = event.event_id;
            programs
                .entry(ProgramModel::make_id(eid, sid, nid))
                .or_insert(ProgramModel::new(eid, sid, nid))
                .update(event);
        }
    }
}

impl From<EitSection> for EpgSection {
    fn from(section: EitSection) -> Self {
        EpgSection {
            version: section.version_number,
            events: section.events,
        }
    }
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EitSection {
    original_network_id: u16,
    transport_stream_id: u16,
    service_id: u16,
    table_id: u16,
    section_number: u8,
    last_section_number: u8,
    segment_last_section_number: u8,
    version_number: u8,
    events: Vec<EitEvent>,
}

impl EitSection {
    fn table_index(&self) -> usize {
        self.table_id as usize - 0x50
    }

    fn segment_index(&self) -> usize {
        self.section_number as usize / 8
    }

    fn section_index(&self) -> usize {
        self.section_number as usize % 8
    }

    fn last_section_index(&self) -> usize {
        self.segment_last_section_number as usize % 8
    }

    fn epg_schedule_id(&self) -> EpgScheduleId {
        (self.original_network_id,
         self.transport_stream_id,
         self.service_id).into()
    }
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EitEvent {
    event_id: u16,
    #[serde(with = "serde_jst")]
    start_time: DateTime<Jst>,
    #[serde(with = "serde_duration_in_millis")]
    duration: Duration,
    scrambled: bool,
    descriptors: Vec<EitDescriptor>,
}

impl EitEvent {
    fn end_time(&self) -> DateTime<Jst> {
        self.start_time + self.duration
    }

    fn is_overnight_event(&self, midnight: DateTime<Jst>) -> bool {
        self.start_time < midnight && self.end_time() > midnight
    }
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
#[serde(tag = "$type")]
enum EitDescriptor {
    #[serde(rename_all = "camelCase")]
    ShortEvent {
        event_name: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Component {
        stream_content: u8,
        component_type: u8,
    },
    #[serde(rename_all = "camelCase")]
    AudioComponent {
        component_type: u8,
        sampling_rate: u8,
    },
    #[serde(rename_all = "camelCase")]
    Content {
        nibbles: Vec<(u8, u8, u8, u8)>,
    },
    #[serde(rename_all = "camelCase")]
    ExtendedEvent {
        items: Vec<(String, String)>,
    },
}

#[derive(Clone)]
#[derive(Deserialize, Serialize)]
pub struct EpgChannel {
    pub name: String,
    #[serde(rename = "type")]
    pub channel_type: ChannelType,
    pub channel: String,
    pub excluded_services: Vec<u16>,
}

impl From<ChannelConfig> for EpgChannel {
    fn from(config: ChannelConfig) -> Self {
        EpgChannel {
            name: config.name,
            channel_type: config.channel_type,
            channel: config.channel,
            excluded_services: config.excluded_services,
        }
    }
}

impl Into<OpenTunerBy> for EpgChannel {
    fn into(self) -> OpenTunerBy {
        OpenTunerBy::Channel {
            channel_type: self.channel_type,
            channel: self.channel,
        }
    }
}

impl Into<ServiceChannelModel> for EpgChannel {
    fn into(self) -> ServiceChannelModel {
        ServiceChannelModel {
            channel_type: self.channel_type,
            channel: self.channel,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsService {
    nid: u16,
    tsid: u16,
    sid: u16,
    #[serde(rename = "type")]
    service_type: u16,
    #[serde(default)]
    logo_id: i16,
    #[serde(default)]
    remote_control_key_id: u16,
    name: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpgService {
    nid: u16,
    tsid: u16,
    sid: u16,
    #[serde(rename = "type")]
    service_type: u16,
    #[serde(default)]
    logo_id: i16,
    #[serde(default)]
    remote_control_key_id: u16,
    name: String,
    channel: EpgChannel,
}

impl EpgService {
    #[inline]
    fn schedule_id(&self) -> EpgScheduleId {
        EpgScheduleId::from((self.nid, self.tsid, self.sid))
    }
}

impl From<(&EpgChannel, &TsService)> for EpgService {
    fn from((ch, sv): (&EpgChannel, &TsService)) -> Self {
        EpgService {
            nid: sv.nid,
            tsid: sv.tsid,
            sid: sv.sid,
            service_type: sv.service_type,
            logo_id: sv.logo_id,
            remote_control_key_id: sv.remote_control_key_id,
            name: sv.name.clone(),
            channel: ch.clone(),
        }
    }
}

impl Into<ServiceModel> for EpgService {
    fn into(self) -> ServiceModel {
        ServiceModel {
            id: ServiceModel::make_id(self.sid, self.nid),
            service_id: self.sid,
            network_id: self.nid,
            service_type: self.service_type,
            logo_id: self.logo_id,
            remote_control_key_id: self.remote_control_key_id,
            name: self.name,
            channel: self.channel.into(),
            has_logo_data: false,
        }
    }
}

impl ProgramModel {
    fn update(&mut self, event: &EitEvent) {
        self.start_at = event.start_time.clone();
        self.duration = event.duration.clone();
        self.is_free = !event.scrambled;
        for desc in event.descriptors.iter() {
            match desc {
                EitDescriptor::ShortEvent { event_name, text } => {
                    self.name = Some(event_name.clone());
                    self.description = Some(text.clone());
                }
                EitDescriptor::Component { stream_content, component_type } => {
                    self.video = Some(
                        EpgVideoInfo::new(*stream_content, *component_type));
                }
                EitDescriptor::AudioComponent {
                    component_type, sampling_rate } => {
                    self.audio = Some(
                        EpgAudioInfo::new(*component_type, *sampling_rate));
                }
                EitDescriptor::Content { nibbles } => {
                    self.genres = Some(nibbles.iter()
                                       .map(|nibble| EpgGenre::new(*nibble))
                                       .collect());
                }
                EitDescriptor::ExtendedEvent { items } => {
                    let mut map = IndexMap::new();
                    map.extend(items.clone());
                    self.extended = Some(map);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Date, TimeZone};
    use serde_yaml;

    #[test]
    fn test_epg_prepare_schedule() {
        let sched_id = EpgScheduleId::from((1, 2, 3));
        let channel_type = ChannelType::GR;
        let services = vec![create_epg_service(sched_id, channel_type)];
        let config = create_config();

        let mut epg = Epg::new(&config);
        epg.prepare_schedules(&services, Jst::now());
        assert_eq!(epg.schedules.len(), 1);
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        let mut epg = Epg::new(&config);
        let sched =
            create_epg_schedule_with_overnight_events(sched_id, channel_type);
        epg.schedules.insert(sched_id, sched);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 13).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 14).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 4);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 15).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 16).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 17).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 18).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 1);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 19).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 20).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 21).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);

        epg.prepare_schedules(
            &services, Jst.ymd(2019, 10, 22).and_hms(0, 0, 0));
        assert_eq!(epg.schedules[&sched_id].overnight_events.len(), 0);
    }

    #[test]
    fn test_epg_schedule_update() {
        let sched_id = EpgScheduleId::from((1, 2, 3));
        let mut sched = create_epg_schedule(sched_id, ChannelType::GR);

        sched.update(EitSection {
            original_network_id: sched_id.nid(),
            transport_stream_id: sched_id.tsid(),
            service_id: sched_id.sid(),
            table_id: 0x50,
            section_number: 0x00,
            last_section_number: 0xF8,
            segment_last_section_number: 0x00,
            version_number: 1,
            events: Vec::new(),
        });
        assert!(sched.tables[0].is_some());
    }

    #[test]
    fn test_epg_schedule_save_overnight_events() {
        let mut sched = create_epg_schedule_with_overnight_events(
            EpgScheduleId::from((1, 2, 3)), ChannelType::GR);
        sched.save_overnight_events(Jst.ymd(2019, 10, 13).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 14).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 4);

        sched.save_overnight_events(Jst.ymd(2019, 10, 15).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 16).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 17).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 18).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 1);

        sched.save_overnight_events(Jst.ymd(2019, 10, 19).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 20).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 21).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);

        sched.save_overnight_events(Jst.ymd(2019, 10, 22).and_hms(0, 0, 0));
        assert_eq!(sched.overnight_events.len(), 0);
    }

    #[test]
    fn test_epg_table_update() {
        let mut table: EpgTable = Default::default();

        table.update(EitSection {
            original_network_id: 1,
            transport_stream_id: 2,
            service_id: 3,
            table_id: 0x50,
            section_number: 0x00,
            last_section_number: 0xF8,
            segment_last_section_number: 0x00,
            version_number: 1,
            events: Vec::new(),
        });
        assert!(table.segments[0].sections[0].is_some());
    }

    #[test]
    fn test_epg_table_collect_overnight_events() {
        let table = create_epg_table_with_overnight_events(
            Jst.ymd(2019, 10, 13));
        let events = table.collect_overnight_events(
            Jst.ymd(2019, 10, 14).and_hms(0, 0, 0), Vec::new());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, 2);
    }

    #[test]
    fn test_epg_segment_update() {
        let mut segment: EpgSegment = Default::default();

        segment.update(EitSection {
            original_network_id: 1,
            transport_stream_id: 2,
            service_id: 3,
            table_id: 0x50,
            section_number: 0x01,
            last_section_number: 0xF8,
            segment_last_section_number: 0x01,
            version_number: 1,
            events: Vec::new(),
        });
        assert!(segment.sections[0].is_none());
        assert!(segment.sections[1].is_some());

        segment.update(EitSection {
            original_network_id: 1,
            transport_stream_id: 2,
            service_id: 3,
            table_id: 0x50,
            section_number: 0x00,
            last_section_number: 0xF8,
            segment_last_section_number: 0x00,
            version_number: 1,
            events: Vec::new(),
        });
        assert!(segment.sections[0].is_some());
        assert!(segment.sections[1].is_none());
    }

    #[test]
    fn test_epg_segment_collect_overnight_events() {
        let segment = create_epg_segment_with_overnight_events(
            Jst.ymd(2019, 10, 13));
        let events = segment.collect_overnight_events(
            Jst.ymd(2019, 10, 14).and_hms(0, 0, 0), Vec::new());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, 2);
    }

    #[test]
    fn test_epg_section_collect_overnight_events() {
        let section = create_epg_section_with_overnight_events(
            Jst.ymd(2019, 10, 13));
        let events = section.collect_overnight_events(
            Jst.ymd(2019, 10, 14).and_hms(0, 0, 0), Vec::new());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, 2);
    }

    #[test]
    fn test_eit_event_is_overnight_event() {
        let event = EitEvent {
            event_id: 0,
            start_time: Jst.ymd(2019, 10, 13).and_hms(23, 59, 59),
            duration: Duration::seconds(2),
            scrambled: false,
            descriptors: Vec::new(),
        };
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 12).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 13).and_hms(0, 0, 0)));
        assert!(event.is_overnight_event(
            Jst.ymd(2019, 10, 14).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 15).and_hms(0, 0, 0)));

        let event = EitEvent {
            event_id: 0,
            start_time: Jst.ymd(2019, 10, 13).and_hms(23, 59, 59),
            duration: Duration::seconds(1),
            scrambled: false,
            descriptors: Vec::new(),
        };
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 12).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 13).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 14).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 15).and_hms(0, 0, 0)));

        let event = EitEvent {
            event_id: 0,
            start_time: Jst.ymd(2019, 10, 13).and_hms(23, 59, 58),
            duration: Duration::seconds(1),
            scrambled: false,
            descriptors: Vec::new(),
        };
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 12).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 13).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 14).and_hms(0, 0, 0)));
        assert!(!event.is_overnight_event(
            Jst.ymd(2019, 10, 15).and_hms(0, 0, 0)));
    }

    fn create_config() -> Config {
        serde_yaml::from_str::<Config>(r#"
            tools:
              scan-services: scan-services
              collect-eits: collect-eits
              filter-service: filter-service
              filter-program: filter-program
            epg-cache-dir: /tmp/epg
        "#).unwrap()
    }

    fn create_epg_service(
        sched_id: EpgScheduleId, channel_type: ChannelType) -> EpgService {
        EpgService {
            nid: sched_id.nid(),
            tsid: sched_id.tsid(),
            sid: sched_id.sid(),
            service_type: 1,
            logo_id: 0,
            remote_control_key_id: 0,
            name: "Service".to_string(),
            channel: EpgChannel {
                name: "Ch".to_string(),
                channel_type,
                channel: "ch".to_string(),
                excluded_services: Vec::new(),
            }
        }
    }

    fn create_epg_schedule(
        sched_id: EpgScheduleId, channel_type: ChannelType) -> EpgSchedule {
        let sv = create_epg_service(sched_id, channel_type);
        EpgSchedule::new(&sv)
    }

    fn create_epg_schedule_with_overnight_events(
        sched_id: EpgScheduleId, channel_type: ChannelType) -> EpgSchedule {
        let mut sched = create_epg_schedule(sched_id, channel_type);
        sched.updated_at = Jst.ymd(2019, 10, 13).and_hms(0, 0, 0);
        sched.tables[0] = Some(Box::new(
            create_epg_table_with_overnight_events(Jst.ymd(2019, 10, 13))));
        sched.tables[1] = Some(Box::new(
            create_epg_table_with_overnight_events(Jst.ymd(2019, 10, 17))));
        sched.tables[8] = Some(Box::new(
            create_epg_table_with_overnight_events(Jst.ymd(2019, 10, 13))));
        sched.tables[16] = Some(Box::new(
            create_epg_table_with_overnight_events(Jst.ymd(2019, 10, 13))));
        sched.tables[24] = Some(Box::new(
            create_epg_table_with_overnight_events(Jst.ymd(2019, 10, 13))));
        sched
    }

    fn create_epg_table_with_overnight_events(date: Date<Jst>) -> EpgTable {
        let mut table = EpgTable::default();
        table.segments[7] = create_epg_segment_with_overnight_events(date);
        table
    }

    fn create_epg_segment_with_overnight_events(date: Date<Jst>) -> EpgSegment {
        let mut segment = EpgSegment::default();
        segment.sections[0] =
            Some(EpgSection { version: 1, events: Vec::new() });
        segment.sections[1] =
            Some(create_epg_section_with_overnight_events(date));
        segment
    }

    fn create_epg_section_with_overnight_events(date: Date<Jst>) -> EpgSection {
        EpgSection {
            version: 1,
            events: vec![
                EitEvent {
                    event_id: 1,
                    start_time: date.and_hms(23, 0, 0),
                    duration: Duration::minutes(30),
                    scrambled: false,
                    descriptors: Vec::new(),
                },
                EitEvent {
                    event_id: 2,
                    start_time: date.and_hms(23, 30, 0),
                    duration: Duration::hours(1),
                    scrambled: false,
                    descriptors: Vec::new(),
                },
            ]
        }
    }
}