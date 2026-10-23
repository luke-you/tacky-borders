use crate::animations::AnimationsConfig;
use crate::colors::ColorBrushConfig;
use crate::effects::EffectsConfig;
use crate::komorebi::KomorebiColorsConfig;
use crate::render_backend::RenderBackendConfig;
use crate::utils::{OwnedHANDLE, get_adjusted_radius, get_window_corner_preference};
use crate::{APP_STATE, BG_SERVICES, DirectXDevices, IS_WINDOWS_11};
use anyhow::{Context, anyhow};
#[cfg(not(test))]
use dirs::home_dir;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::{env, iter, ptr, thread, time};
use windows::Win32::Foundation::{ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, HANDLE, HWND};
use windows::Win32::Graphics::Dwm::{
    DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::CancelIoEx;
use windows::core::PCWSTR;

const DEFAULT_CONFIG: &str = include_str!("resources/config.yaml");
const CONFIG_RELOAD_IDLE: u8 = 0;
const CONFIG_RELOAD_PENDING: u8 = 1;
const CONFIG_RELOAD_DIRTY: u8 = 2;
static CONFIG_RELOAD_STATE: AtomicU8 = AtomicU8::new(CONFIG_RELOAD_IDLE);
static LAST_WATCHER_REJECTION: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn unit_test_config_dir() -> PathBuf {
    static CONFIG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CONFIG_DIR
        .get_or_init(|| {
            let nonce = time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .expect("system clock is after UNIX epoch")
                .as_nanos();
            env::temp_dir().join(format!(
                "tacky-borders-unit-tests-{}-{nonce}",
                std::process::id()
            ))
        })
        .clone()
}

/// The config.yaml definition
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub watch_config_changes: bool,
    #[serde(default = "serde_default_bool::<true>")]
    pub enable_logging: bool,
    #[serde(default = "serde_default_bool::<true>")]
    pub enable_ipc_server: bool,
    #[serde(default)]
    #[serde(alias = "rendering_backend")]
    pub render_backend: RenderBackendConfig,
    #[serde(default = "serde_default_global")]
    pub global: Global,
    #[serde(default)]
    pub window_rules: Vec<WindowRule>,
}

// Show borders even if the config.yaml is completely empty
// NOTE: This is intentionally kept separate from the Default trait because I want the
// width/offset zeroed out when config deserialization fails and falls back to Config::default()
fn serde_default_global() -> Global {
    Global {
        border_width: WidthConfig::serde_default(),
        border_offset: OffsetConfig::serde_default(),
        ..Default::default()
    }
}

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Global {
    #[serde(default = "WidthConfig::serde_default")]
    pub border_width: WidthConfig,
    #[serde(default = "OffsetConfig::serde_default")]
    pub border_offset: OffsetConfig,
    #[serde(default)]
    pub border_radius: RadiusConfig,
    #[serde(default)]
    pub border_z_order: ZOrderMode,
    #[serde(default = "serde_default_bool::<true>")]
    pub follow_native_border: bool,
    #[serde(default)]
    pub active_color: ColorBrushConfig,
    #[serde(default)]
    pub inactive_color: ColorBrushConfig,
    #[serde(default)]
    pub komorebi_colors: KomorebiColorsConfig,
    #[serde(default)]
    pub animations: AnimationsConfig,
    #[serde(default)]
    pub effects: EffectsConfig,
    #[serde(alias = "init_delay")]
    #[serde(default = "serde_default_u64::<250>")]
    pub initialize_delay: u64, // Adjust delay when creating new windows/borders
    #[serde(alias = "restore_delay")]
    #[serde(default = "serde_default_u64::<200>")]
    pub unminimize_delay: u64, // Adjust delay when restoring minimized windows
}

pub fn serde_default_u64<const V: u64>() -> u64 {
    V
}

pub fn serde_default_i32<const V: i32>() -> i32 {
    V
}

// f32 cannot be a const, so we have to do the following instead
pub fn serde_default_f32<const V: i32>() -> f32 {
    V as f32
}

pub fn serde_default_bool<const V: bool>() -> bool {
    V
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    #[serde(rename = "match")]
    pub kind: Option<MatchKind>,
    pub name: Option<String>,
    pub strategy: Option<MatchStrategy>,
    pub border_width: Option<WidthConfig>,
    pub border_offset: Option<OffsetConfig>,
    pub border_radius: Option<RadiusConfig>,
    pub border_z_order: Option<ZOrderMode>,
    pub follow_native_border: Option<bool>,
    pub active_color: Option<ColorBrushConfig>,
    pub inactive_color: Option<ColorBrushConfig>,
    pub komorebi_colors: Option<KomorebiColorsConfig>,
    pub animations: Option<AnimationsConfig>,
    pub effects: Option<EffectsConfig>,
    #[serde(alias = "init_delay")]
    pub initialize_delay: Option<u64>,
    #[serde(alias = "restore_delay")]
    pub unminimize_delay: Option<u64>,
    pub enabled: Option<EnableMode>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MatchKind {
    Title,
    Class,
    Process,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MatchStrategy {
    Equals,
    Contains,
    Regex,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    source: Config,
    window_rules: Vec<ResolvedWindowRule>,
}

impl PartialEq for RuntimeConfig {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::fallback()
    }
}

#[cfg(test)]
impl std::ops::DerefMut for RuntimeConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.source
    }
}

impl Deref for RuntimeConfig {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWindowRule {
    source: WindowRule,
    kind: MatchKind,
    matcher: WindowMatcher,
}

#[derive(Clone, Debug)]
enum WindowMatcher {
    Equals(String),
    Contains(String),
    Regex(Regex),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum ReloadStage {
    Read,
    Parse,
    Validate,
    PrepareServices,
    PrepareRenderer,
    StalePlan,
    Commit,
}

#[derive(Debug)]
pub enum ReloadOutcome {
    Applied {
        changed: bool,
        generation: u64,
    },
    Rejected {
        stage: ReloadStage,
        error: anyhow::Error,
        report: bool,
    },
}

pub struct ReloadCompletion {
    pub outcome: ReloadOutcome,
    pub retired: crate::RetiredResources,
}

impl RuntimeConfig {
    pub fn from_source(source: Config) -> anyhow::Result<Self> {
        source.global.animations.validate("global.animations")?;
        for (index, rule) in source.window_rules.iter().enumerate() {
            if let Some(animations) = &rule.animations {
                animations.validate(&format!("window_rules[{}].animations", index + 1))?;
            }
        }

        let window_rules = source
            .window_rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let display_index = index + 1;
                let kind = rule.kind.ok_or_else(|| {
                    anyhow!("window_rules[{display_index}].match is required")
                })?;
                let name = rule.name.as_ref().ok_or_else(|| {
                    anyhow!("window_rules[{display_index}].name is required")
                })?;
                let strategy = rule.strategy.unwrap_or(MatchStrategy::Equals);
                let matcher = match strategy {
                    MatchStrategy::Equals => WindowMatcher::Equals(name.to_lowercase()),
                    MatchStrategy::Contains => WindowMatcher::Contains(name.to_lowercase()),
                    MatchStrategy::Regex => WindowMatcher::Regex(
                        Regex::new(name).with_context(|| {
                            format!(
                                "window_rules[{display_index}] ({kind:?}, {strategy:?}) has invalid pattern {name:?}"
                            )
                        })?,
                    ),
                };

                Ok(ResolvedWindowRule {
                    source: rule.clone(),
                    kind,
                    matcher,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            source,
            window_rules,
        })
    }

    pub fn parse(contents: &str) -> anyhow::Result<Self> {
        let source = serde_yaml_ng::from_str(contents).map_err(anyhow::Error::new)?;
        Self::from_source(source)
    }

    pub fn load() -> anyhow::Result<Self> {
        Self::parse(&Config::read_contents()?)
    }

    pub fn fallback() -> Self {
        Self {
            source: Config::default(),
            window_rules: Vec::new(),
        }
    }

    pub fn source(&self) -> &Config {
        &self.source
    }

    pub(crate) fn global_mut(&mut self) -> &mut Global {
        &mut self.source.global
    }

    pub(crate) fn window_rules(&self) -> &[ResolvedWindowRule] {
        &self.window_rules
    }
}

impl ResolvedWindowRule {
    pub(crate) fn source(&self) -> &WindowRule {
        &self.source
    }

    pub(crate) fn kind(&self) -> MatchKind {
        self.kind
    }

    pub(crate) fn is_match(&self, window_name: &str) -> bool {
        match &self.matcher {
            WindowMatcher::Equals(pattern) => window_name.to_lowercase() == *pattern,
            WindowMatcher::Contains(pattern) => window_name.to_lowercase().contains(pattern),
            WindowMatcher::Regex(regex) => regex.is_match(window_name),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct WidthConfig(f32);

impl WidthConfig {
    pub fn new(width: f32) -> Self {
        Self(width)
    }

    // TODO: Maybe rename this and other to_x methods to to_raw or smth idk
    /// Returns a DPI-adjusted raw width value
    pub fn to_width(&self, dpi: f32) -> i32 {
        (self.0 * dpi / 96.0).round() as i32
    }

    fn serde_default() -> Self {
        Self(4.0)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OffsetConfig {
    Uniform(i32),
    PerSide(Offset),
}

impl OffsetConfig {
    pub fn new(offset: i32) -> Self {
        Self::Uniform(offset)
    }

    /// Returns a DPI-adjusted offset for each side
    pub fn to_offset(&self, dpi: f32) -> Offset {
        let scale = |value: i32| (value as f32 * dpi / 96.0).round() as i32;
        match *self {
            Self::Uniform(offset) => Offset::new(scale(offset)),
            Self::PerSide(offset) => Offset {
                top: scale(offset.top),
                left: scale(offset.left),
                right: scale(offset.right),
                bottom: scale(offset.bottom),
            },
        }
    }

    fn serde_default() -> Self {
        Self::Uniform(-1)
    }
}

impl Default for OffsetConfig {
    fn default() -> Self {
        Self::Uniform(0)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Offset {
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub right: i32,
    #[serde(default)]
    pub bottom: i32,
}

impl Offset {
    pub fn new(offset: i32) -> Self {
        Self {
            top: offset,
            left: offset,
            right: offset,
            bottom: offset,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum RadiusConfig {
    #[default]
    Auto,
    Square,
    Round,
    RoundSmall,
    #[serde(untagged)]
    Custom(f32),
}

impl RadiusConfig {
    pub fn to_radius(&self, border_width: i32, dpi: u32, tracking_window: HWND) -> f32 {
        match self {
            // We also check Custom(-1.0) for legacy reasons (don't wanna break anyone's old config)
            RadiusConfig::Auto | RadiusConfig::Custom(-1.0) => {
                // I believe this will error on Windows 10, so we'll just use a default
                match get_window_corner_preference(tracking_window).unwrap_or(DWMWCP_DEFAULT) {
                    DWMWCP_DEFAULT => {
                        if *IS_WINDOWS_11 {
                            get_adjusted_radius(8.0, dpi, border_width)
                        } else {
                            0.0
                        }
                    }
                    DWMWCP_DONOTROUND => 0.0,
                    DWMWCP_ROUND => get_adjusted_radius(8.0, dpi, border_width),
                    DWMWCP_ROUNDSMALL => get_adjusted_radius(4.0, dpi, border_width),
                    _ => 0.0,
                }
            }
            RadiusConfig::Square => 0.0,
            RadiusConfig::Round => get_adjusted_radius(8.0, dpi, border_width),
            RadiusConfig::RoundSmall => get_adjusted_radius(4.0, dpi, border_width),
            RadiusConfig::Custom(radius) => get_adjusted_radius(*radius, dpi, border_width),
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum EnableMode {
    #[default]
    Auto,
    #[serde(untagged)]
    Bool(bool),
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum ZOrderMode {
    #[default]
    AboveWindow,
    BelowWindow,
}

impl Config {
    pub fn create() -> anyhow::Result<Self> {
        let contents = Self::read_contents()?;
        serde_yaml_ng::from_str(&contents).map_err(anyhow::Error::new)
    }

    fn read_contents() -> anyhow::Result<String> {
        Self::read_contents_with_context(&crate::command_context::CommandContext::default())
    }

    fn read_contents_with_context(
        context: &crate::command_context::CommandContext,
    ) -> anyhow::Result<String> {
        context.check()?;
        let config_dir = Self::get_dir()?;
        let config_path = config_dir.join("config.yaml");

        // When saving files with a text editor like VSCode or Neovim, there may be a small time
        // period where the target file is empty or doesn't exist, so we'll use some retries.
        context.check()?;
        let mut exists = fs::exists(&config_path).context("could not check if config exists")?;
        for _ in 0..2 {
            if !exists {
                debug!("config does not exist; attempting to check again");
                context.wait(time::Duration::from_millis(20))?;
                exists = fs::exists(&config_path).context("could not check if config exists")?;
            } else {
                break;
            }
        }

        // If the config.yaml does not exist, try to create it
        if !exists {
            context.check()?;
            let default_contents = DEFAULT_CONFIG.as_bytes();
            fs::write(&config_path, default_contents)
                .context("could not create default config.yaml")?;

            info!("generating default config in {}", config_dir.display());
        }

        // We also implement retries here for the same reasons listed earlier
        context.check()?;
        let mut contents = fs::read_to_string(&config_path).context("could not read config")?;
        for _ in 0..2 {
            if contents.is_empty() {
                debug!("config is empty; attempting to read again");
                context.wait(time::Duration::from_millis(20))?;
                contents = fs::read_to_string(&config_path).context("could not read config")?;
            } else {
                break;
            }
        }

        Ok(contents)
    }

    pub fn get_dir() -> anyhow::Result<PathBuf> {
        #[cfg(test)]
        let config_dir = unit_test_config_dir();
        #[cfg(not(test))]
        let config_dir = env::var("TACKY_BORDERS_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|env_err| {
                home_dir()
                    .map(|dir| dir.join(".config").join("tacky-borders"))
                    .ok_or(anyhow!("could not find home dir, and could not access TACKY_BORDERS_CONFIG_HOME env var: {env_err}"))
            })?;

        if !config_dir.exists() {
            DirBuilder::new()
                .recursive(true)
                .create(&config_dir)
                .context("could not create config directory")?;
        };

        Ok(config_dir)
    }

    pub fn reload(origin: crate::ReloadOrigin) -> ReloadCompletion {
        Self::reload_with_context(origin, &crate::command_context::CommandContext::default())
    }

    pub fn reload_with_context(
        origin: crate::ReloadOrigin,
        context: &crate::command_context::CommandContext,
    ) -> ReloadCompletion {
        if let Err(error) = context.check() {
            return reject_reload(ReloadStage::Commit, error, origin, 0);
        }
        let contents = match Self::read_contents_with_context(context) {
            Ok(contents) => contents,
            Err(error) => {
                return reject_reload(
                    ReloadStage::Read,
                    error,
                    origin,
                    rejection_fingerprint("", ReloadStage::Read, "read failed"),
                );
            }
        };
        if let Err(error) = context.check() {
            return reject_reload(ReloadStage::Parse, error, origin, 0);
        }
        let source = match serde_yaml_ng::from_str::<Config>(&contents) {
            Ok(source) => source,
            Err(error) => {
                return reject_reload(
                    ReloadStage::Parse,
                    anyhow::Error::new(error),
                    origin,
                    rejection_fingerprint(&contents, ReloadStage::Parse, "parse failed"),
                );
            }
        };
        if let Err(error) = context.check() {
            return reject_reload(ReloadStage::Validate, error, origin, 0);
        }
        let candidate = match RuntimeConfig::from_source(source) {
            Ok(candidate) => candidate,
            Err(error) => {
                return reject_reload(
                    ReloadStage::Validate,
                    error,
                    origin,
                    rejection_fingerprint(&contents, ReloadStage::Validate, "validation failed"),
                );
            }
        };
        for attempt in 0..=1 {
            let snapshot = (|| -> anyhow::Result<_> {
                let active = context.read(&APP_STATE.config, "configuration")?;
                let generation = APP_STATE.config_generation();
                let services = context
                    .lock(&BG_SERVICES, "background services")?
                    .presence();
                let directx_present = context
                    .read(&APP_STATE.directx_devices, "render device state")?
                    .is_some();
                Ok((active.clone(), generation, services, directx_present))
            })();
            let (active, base_generation, service_presence, directx_present) = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return reject_reload(
                        ReloadStage::Commit,
                        error,
                        origin,
                        rejection_fingerprint(
                            &contents,
                            ReloadStage::Commit,
                            "could not snapshot active state",
                        ),
                    );
                }
            };

            let renderer_needed = candidate.render_backend == RenderBackendConfig::V2;
            let services_satisfied =
                crate::BackgroundServices::services_satisfy(service_presence, candidate.source());
            if active == candidate && services_satisfied && directx_present == renderer_needed {
                let accepted = (|| -> anyhow::Result<()> {
                    let mut rejection =
                        context.lock(&LAST_WATCHER_REJECTION, "watcher rejection state")?;
                    context.begin_commit()?;
                    *rejection = None;
                    Ok(())
                })();
                if let Err(error) = accepted {
                    return reject_reload(ReloadStage::Commit, error, origin, 0);
                }
                return ReloadCompletion {
                    outcome: ReloadOutcome::Applied {
                        changed: false,
                        generation: base_generation,
                    },
                    retired: crate::RetiredResources {
                        services: crate::RetiredServices::default(),
                        directx_devices: None,
                    },
                };
            }

            let service_plan = match crate::BackgroundServices::prepare_reload(
                service_presence,
                candidate.source(),
                context,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    return reject_reload(
                        ReloadStage::PrepareServices,
                        error.context("prepare background services"),
                        origin,
                        rejection_fingerprint(
                            &contents,
                            ReloadStage::PrepareServices,
                            "service preparation failed",
                        ),
                    );
                }
            };
            let renderer_plan =
                match prepare_renderer_plan(renderer_needed, directx_present, || {
                    context.check()?;
                    DirectXDevices::new(&APP_STATE.render_factory).map_err(anyhow::Error::from)
                }) {
                    Ok(plan) => plan,
                    Err(error) => {
                        drop(service_plan);
                        return reject_reload(
                            ReloadStage::PrepareRenderer,
                            error.context("prepare DirectX devices"),
                            origin,
                            rejection_fingerprint(
                                &contents,
                                ReloadStage::PrepareRenderer,
                                "renderer preparation failed",
                            ),
                        );
                    }
                };
            let next_generation = match base_generation.checked_add(1) {
                Some(generation) => generation,
                None => {
                    return reject_reload(
                        ReloadStage::Commit,
                        anyhow!("configuration generations exhausted"),
                        origin,
                        rejection_fingerprint(
                            &contents,
                            ReloadStage::Commit,
                            "configuration generations exhausted",
                        ),
                    );
                }
            };

            let commit = (|| -> Result<_, ReloadCommitError> {
                // Appearance mutations acquire these locks in the same order.
                let mut sequence = context.lock(&APP_STATE.appearance, "appearance gate")?;
                let mut config = context.write(&APP_STATE.config, "configuration")?;
                let mut services = context.lock(&BG_SERVICES, "background services")?;
                let mut devices =
                    context.write(&APP_STATE.directx_devices, "render device state")?;

                if APP_STATE.config_generation() != base_generation
                    || *config != active
                    || services.presence() != service_presence
                    || devices.is_some() != directx_present
                {
                    drop(devices);
                    drop(services);
                    drop(config);
                    drop(sequence);
                    return Err(ReloadCommitError::Stale);
                }

                let revision = sequence
                    .config_revision
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("configuration revisions exhausted"))?;
                let mut rejection =
                    context.lock(&LAST_WATCHER_REJECTION, "watcher rejection state")?;
                context.begin_commit()?;
                let command = sequence
                    .next_command()
                    .ok_or_else(|| anyhow!("appearance commands exhausted"))?;

                let retired_services = services.commit_reload(service_plan);
                let retired_devices = match renderer_plan {
                    RendererPlan::Keep => None,
                    RendererPlan::Install(new_devices) => devices.replace(new_devices),
                    RendererPlan::Remove => devices.take(),
                };
                *config = candidate.clone();
                sequence.config_revision = revision;
                sequence.global_stamps = crate::border_updates::AppearanceStamps {
                    active: command,
                    inactive: command,
                    width: command,
                    offset: command,
                    radius: command,
                };
                APP_STATE.publish_config_generation(next_generation);
                *rejection = None;
                drop(devices);
                drop(services);
                drop(config);
                drop(sequence);
                Ok(crate::RetiredResources {
                    services: retired_services,
                    directx_devices: retired_devices,
                })
            })();

            match commit {
                Ok(retired) => {
                    return ReloadCompletion {
                        outcome: ReloadOutcome::Applied {
                            changed: true,
                            generation: next_generation,
                        },
                        retired,
                    };
                }
                Err(ReloadCommitError::Stale) if attempt == 0 => {
                    continue;
                }
                Err(ReloadCommitError::Stale) => {
                    return reject_reload(
                        ReloadStage::StalePlan,
                        anyhow!("reload plan became stale"),
                        origin,
                        rejection_fingerprint(
                            &contents,
                            ReloadStage::StalePlan,
                            "plan remained stale after replan",
                        ),
                    );
                }
                Err(ReloadCommitError::Failed(error)) => {
                    return reject_reload(
                        ReloadStage::Commit,
                        error,
                        origin,
                        rejection_fingerprint(&contents, ReloadStage::Commit, "commit failed"),
                    );
                }
            }
        }
        reject_reload(
            ReloadStage::StalePlan,
            anyhow!("reload plan remained stale after one retry"),
            origin,
            rejection_fingerprint(
                &contents,
                ReloadStage::StalePlan,
                "plan remained stale after one retry",
            ),
        )
    }

    pub fn is_config_watcher_enabled(&self) -> bool {
        self.watch_config_changes
    }

    pub fn is_komorebi_integration_enabled(&self) -> bool {
        self.global.komorebi_colors.enabled
            || self.window_rules.iter().any(|rule| {
                rule.komorebi_colors
                    .as_ref()
                    .map(|komocolors| komocolors.enabled)
                    .unwrap_or(false)
            })
    }

    pub fn is_theme_aware_enabled(&self) -> bool {
        Self::is_color_theme_aware(&self.global.active_color)
            || Self::is_color_theme_aware(&self.global.inactive_color)
            || self.window_rules.iter().any(|rule| {
                rule.active_color
                    .as_ref()
                    .is_some_and(Self::is_color_theme_aware)
                    || rule
                        .inactive_color
                        .as_ref()
                        .is_some_and(Self::is_color_theme_aware)
            })
    }

    fn is_color_theme_aware(config: &ColorBrushConfig) -> bool {
        matches!(config, ColorBrushConfig::ThemeAware(_))
    }

    pub fn is_ipc_server_enabled(&self) -> bool {
        self.enable_ipc_server
    }
}

enum RendererPlan<T> {
    Keep,
    Install(T),
    Remove,
}

fn prepare_renderer_plan<T>(
    required: bool,
    present: bool,
    create: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<RendererPlan<T>> {
    match (required, present) {
        (true, false) => create().map(RendererPlan::Install),
        (false, true) => Ok(RendererPlan::Remove),
        _ => Ok(RendererPlan::Keep),
    }
}

enum ReloadCommitError {
    Stale,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for ReloadCommitError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

fn rejection_fingerprint(contents: &str, stage: ReloadStage, description: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    stage.hash(&mut hasher);
    description.hash(&mut hasher);
    hasher.finish()
}

fn reject_reload(
    stage: ReloadStage,
    error: anyhow::Error,
    origin: crate::ReloadOrigin,
    fingerprint: u64,
) -> ReloadCompletion {
    let report = if origin == crate::ReloadOrigin::Watcher {
        let mut last = LAST_WATCHER_REJECTION
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if *last == Some(fingerprint) {
            false
        } else {
            *last = Some(fingerprint);
            true
        }
    } else {
        true
    };
    ReloadCompletion {
        outcome: ReloadOutcome::Rejected {
            stage,
            error,
            report,
        },
        retired: crate::RetiredResources::default(),
    }
}

#[derive(Debug)]
pub struct ConfigWatcher {
    dir_handle: OwnedHANDLE,
    stop_requested: Arc<AtomicBool>,
    thread_handle: Option<JoinHandle<()>>,
}

const FILE_NOTIFY_HEADER_BYTES: usize = 12;

// ReadDirectoryChangesW requires a DWORD-aligned output buffer.
#[repr(align(4))]
struct DirectoryNotificationBuffer([u8; 1024]);

fn notification_end(start: usize, length: usize, field: &str) -> anyhow::Result<usize> {
    start.checked_add(length).with_context(|| {
        format!("directory notification at byte {start}: {field} {length} overflows")
    })
}

fn notification_contains_config(bytes: &[u8], config_name: &str) -> anyhow::Result<bool> {
    let mut offset = 0;
    let mut changed = false;
    while offset < bytes.len() {
        let header_end = notification_end(offset, FILE_NOTIFY_HEADER_BYTES, "header length")?;
        let header = bytes.get(offset..header_end).with_context(|| {
            format!(
                "directory notification at byte {offset}: truncated header in {} returned bytes",
                bytes.len()
            )
        })?;
        let next = usize::try_from(u32::from_ne_bytes(header[..4].try_into()?))?;
        let name_length = usize::try_from(u32::from_ne_bytes(header[8..12].try_into()?))?;
        anyhow::ensure!(
            name_length.is_multiple_of(2),
            "directory notification at byte {offset}: odd FileNameLength {name_length}"
        );
        let record_end = if next == 0 {
            bytes.len()
        } else {
            anyhow::ensure!(
                next >= FILE_NOTIFY_HEADER_BYTES && next.is_multiple_of(4),
                "directory notification at byte {offset}: invalid NextEntryOffset {next}"
            );
            let end = notification_end(offset, next, "NextEntryOffset")?;
            anyhow::ensure!(
                end < bytes.len(),
                "directory notification at byte {offset}: NextEntryOffset {next} \
                 leaves no next record in {} returned bytes",
                bytes.len()
            );
            end
        };
        let name_end = notification_end(header_end, name_length, "FileNameLength")?;
        anyhow::ensure!(
            name_end <= record_end,
            "directory notification at byte {offset}: FileNameLength {name_length} \
             exceeds record ending at byte {record_end} in {} returned bytes",
            bytes.len()
        );
        let name_units = bytes[header_end..name_end]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        let name = String::from_utf16(&name_units).with_context(|| {
            format!("directory notification at byte {offset}: invalid UTF-16 filename")
        })?;
        changed |= name == config_name;
        if next == 0 {
            let padding = (4 - name_end % 4) % 4;
            anyhow::ensure!(
                bytes.len() - name_end <= padding,
                "directory notification at byte {offset}: excess trailing bytes after filename"
            );
            break;
        }
        offset = record_end;
    }
    Ok(changed)
}

impl ConfigWatcher {
    pub fn new(
        config_path: PathBuf,
        debounce_time: u64,
        callback_fn: fn(),
    ) -> anyhow::Result<Self> {
        let config_dir = config_path
            .parent()
            .context("could not get parent dir for config watcher")?;
        let config_dir_vec: Vec<u16> = config_dir
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();

        let dir_handle = {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(config_dir_vec.as_ptr()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    None,
                )
            }
            .context("could not create dir handle for config watcher")?;
            OwnedHANDLE(handle)
        };

        // Convert HANDLE to isize so we can move it into the new thread
        let dir_handle_isize = dir_handle.0.0 as isize;

        // Also initialize these variables so we move them into the new thread
        let config_name = config_path
            .file_name()
            .context("could not get config name for config watcher")?
            .to_owned()
            .into_string()
            .map_err(|_| anyhow!("could not convert config name for config watcher"))?;

        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop_requested = stop_requested.clone();
        let watcher_worker = move || unsafe {
            debug!("entering config watcher thread");

            // Reconvert isize back to HANDLE
            let dir_handle = HANDLE(dir_handle_isize as _);

            let mut buffer = DirectoryNotificationBuffer([0u8; 1024]);
            let mut bytes_returned = 0u32;

            loop {
                if worker_stop_requested.load(Ordering::Acquire) {
                    break;
                }

                if let Err(e) = ReadDirectoryChangesW(
                    dir_handle,
                    buffer.0.as_mut_ptr() as _,
                    buffer.0.len() as u32,
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    Some(ptr::addr_of_mut!(bytes_returned)),
                    None,
                    None,
                ) {
                    if is_expected_watcher_cancellation(
                        &e,
                        worker_stop_requested.load(Ordering::Acquire),
                    ) {
                        debug!("config watcher directory read canceled during shutdown");
                    } else {
                        error!("could not check for changes in config dir: {e}");
                    }
                    break;
                }

                if worker_stop_requested.load(Ordering::Acquire) {
                    break;
                }

                if let Err(error) = Self::process_dir_change_notifs(
                    &buffer.0,
                    bytes_returned,
                    &config_name,
                    callback_fn,
                ) {
                    error!("could not parse config directory notifications: {error:#}");
                }

                if worker_stop_requested.load(Ordering::Acquire) {
                    break;
                }

                // Prevent too many directory checks in quick succession
                // NOTE: if any dir changes are made while the thread is asleep, the OS will hold
                // the operations in queue, so we can immediately check them again after looping
                thread::park_timeout(time::Duration::from_millis(debounce_time));
            }

            debug!("exiting config watcher thread");
        };
        let thread_handle = thread::Builder::new()
            .name("config-watcher".into())
            .spawn(watcher_worker)
            .context("could not start config watcher thread")?;

        Ok(Self {
            dir_handle,
            stop_requested,
            thread_handle: Some(thread_handle),
        })
    }

    fn process_dir_change_notifs(
        buffer: &[u8],
        bytes_returned: u32,
        config_name: &str,
        callback_fn: impl FnOnce(),
    ) -> anyhow::Result<()> {
        let returned = usize::try_from(bytes_returned)
            .context("directory notification byte count does not fit usize")?;
        let bytes = buffer.get(..returned).with_context(|| {
            format!(
                "directory notification byte count {returned} exceeds buffer length {}",
                buffer.len()
            )
        })?;
        if returned == 0 {
            // ReadDirectoryChangesW can discard notifications and succeed with zero bytes.
            callback_fn();
            return Ok(());
        }
        if notification_contains_config(bytes, config_name)? {
            callback_fn();
        }
        Ok(())
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);

        let Some(handle) = self.thread_handle.take() else {
            error!("could not take config watcher thread handle");
            return;
        };
        handle.thread().unpark();

        let cancellation_failed = match unsafe { CancelIoEx(self.dir_handle.0, None) } {
            Ok(()) => false,
            Err(err) if is_operation_not_found(&err) => false,
            Err(err) => {
                error!(
                    "could not cancel i/o operations on {:?} for config watcher: {err:#}",
                    self.dir_handle.0
                );
                true
            }
        };

        join_or_reap_config_watcher(handle, cancellation_failed);
    }
}

fn is_operation_not_found(error: &windows::core::Error) -> bool {
    error.code() == ERROR_NOT_FOUND.to_hresult()
}

fn is_expected_watcher_cancellation(error: &windows::core::Error, stopping: bool) -> bool {
    stopping && error.code() == ERROR_OPERATION_ABORTED.to_hresult()
}

fn join_or_reap_config_watcher(handle: JoinHandle<()>, cancellation_failed: bool) {
    if cancellation_failed || handle.thread().id() == thread::current().id() {
        if let Err(err) = thread::Builder::new()
            .name("config-watcher-reaper".into())
            .spawn(move || log_config_watcher_join(handle.join()))
        {
            error!("could not spawn config watcher reaper: {err}");
        }
    } else {
        log_config_watcher_join(handle.join());
    }
}

fn log_config_watcher_join(result: thread::Result<()>) {
    if let Err(payload) = result {
        crate::border_registry::discard_panic(payload);
        error!("config watcher thread panicked");
    }
}

pub fn config_watcher_callback() {
    crate::utils::LogIfErr::log_if_err(&request_config_reload());
}

fn request_config_reload() -> anyhow::Result<()> {
    request_config_reload_with(|| {
        crate::submit_host_action(crate::HostAction::ReloadConfig {
            origin: crate::ReloadOrigin::Watcher,
            response: None,
            context: None,
        })
    })
}

fn request_config_reload_with(submit: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
    match CONFIG_RELOAD_STATE.compare_exchange(
        CONFIG_RELOAD_IDLE,
        CONFIG_RELOAD_PENDING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            if let Err(error) = submit() {
                CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
                return Err(error);
            }
        }
        Err(CONFIG_RELOAD_PENDING) => {
            let _ = CONFIG_RELOAD_STATE.compare_exchange(
                CONFIG_RELOAD_PENDING,
                CONFIG_RELOAD_DIRTY,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        Err(CONFIG_RELOAD_DIRTY) => {}
        Err(_) => unreachable!("configuration reload state is bounded"),
    }
    Ok(())
}

pub fn finish_config_reload() {
    loop {
        match CONFIG_RELOAD_STATE.load(Ordering::Acquire) {
            CONFIG_RELOAD_IDLE => return,
            CONFIG_RELOAD_PENDING => {
                if CONFIG_RELOAD_STATE
                    .compare_exchange(
                        CONFIG_RELOAD_PENDING,
                        CONFIG_RELOAD_IDLE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return;
                }
            }
            CONFIG_RELOAD_DIRTY => {
                if CONFIG_RELOAD_STATE
                    .compare_exchange(
                        CONFIG_RELOAD_DIRTY,
                        CONFIG_RELOAD_PENDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    if let Err(error) = submit_config_reload_action() {
                        CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
                        error!("could not queue deferred config reload: {error:#}");
                    }
                    return;
                }
            }
            _ => unreachable!("configuration reload state is bounded"),
        }
    }
}

pub fn begin_config_reload() {
    CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
}

fn submit_config_reload_action() -> anyhow::Result<()> {
    crate::submit_host_action(crate::HostAction::ReloadConfig {
        origin: crate::ReloadOrigin::Watcher,
        response: None,
        context: None,
    })
}

#[cfg(test)]
mod runtime_config_tests {
    use super::*;

    #[test]
    fn unit_tests_use_an_isolated_config_directory() {
        let config_dir = Config::get_dir().unwrap();
        assert!(config_dir.starts_with(env::temp_dir()));
        assert_ne!(
            env::var_os("TACKY_BORDERS_CONFIG_HOME").map(PathBuf::from),
            Some(config_dir)
        );
        assert_eq!(Config::read_contents().unwrap(), DEFAULT_CONFIG);
    }

    fn rule(
        kind: Option<MatchKind>,
        name: Option<&str>,
        strategy: Option<MatchStrategy>,
    ) -> WindowRule {
        WindowRule {
            kind,
            name: name.map(str::to_owned),
            strategy,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_and_matches_all_strategies() {
        let source = Config {
            window_rules: vec![
                rule(Some(MatchKind::Title), Some("EDITOR"), None),
                rule(
                    Some(MatchKind::Class),
                    Some("Console"),
                    Some(MatchStrategy::Contains),
                ),
                rule(
                    Some(MatchKind::Process),
                    Some("^Code\\.exe$"),
                    Some(MatchStrategy::Regex),
                ),
            ],
            ..Default::default()
        };

        let runtime = RuntimeConfig::from_source(source).unwrap();
        assert!(runtime.window_rules()[0].is_match("editor"));
        assert!(runtime.window_rules()[1].is_match("WindowsConsoleHost"));
        assert!(runtime.window_rules()[2].is_match("Code.exe"));
        assert!(!runtime.window_rules()[2].is_match("code.exe"));
        assert_eq!(runtime.window_rules()[0].kind(), MatchKind::Title);
    }

    #[test]
    fn rejects_invalid_regex_with_rule_context() {
        for pattern in ["[", "(", "\\", "a{2,1}"] {
            let source = Config {
                window_rules: vec![
                    rule(Some(MatchKind::Title), Some("valid"), None),
                    rule(
                        Some(MatchKind::Process),
                        Some(pattern),
                        Some(MatchStrategy::Regex),
                    ),
                ],
                ..Default::default()
            };

            let error = RuntimeConfig::from_source(source).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("window_rules[2] (Process, Regex)"));
            assert!(message.contains(&format!("{pattern:?}")));
        }
    }

    #[test]
    fn parse_rejects_invalid_regex() {
        let error = RuntimeConfig::parse(
            "window_rules:\n  - match: Title\n    name: \"[\"\n    strategy: Regex\n",
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("window_rules[1] (Title, Regex)"));
    }

    #[test]
    fn rejects_missing_required_rule_fields() {
        let missing_kind = RuntimeConfig::from_source(Config {
            window_rules: vec![rule(None, Some("name"), None)],
            ..Default::default()
        })
        .unwrap_err();
        assert!(format!("{missing_kind:#}").contains("window_rules[1].match is required"));

        let missing_name = RuntimeConfig::from_source(Config {
            window_rules: vec![rule(Some(MatchKind::Title), None, None)],
            ..Default::default()
        })
        .unwrap_err();
        assert!(format!("{missing_name:#}").contains("window_rules[1].name is required"));
    }

    #[test]
    fn fallback_is_resolved_and_equality_uses_source() {
        let fallback = RuntimeConfig::fallback();
        assert!(fallback.window_rules().is_empty());
        assert!(fallback.source().window_rules.is_empty());

        let source = Config {
            window_rules: vec![rule(
                Some(MatchKind::Title),
                Some("^title$"),
                Some(MatchStrategy::Regex),
            )],
            ..Default::default()
        };
        assert_eq!(
            RuntimeConfig::from_source(source.clone()).unwrap(),
            RuntimeConfig::from_source(source).unwrap()
        );
    }

    #[test]
    fn runtime_config_validates_enabled_animation_timing() {
        let valid = RuntimeConfig::parse(
            "global:\n  animations:\n    enabled: true\n    fps: 1000\n    active:\n      - type: Fade\n        duration: 1.0e-45\n",
        )
        .unwrap();
        assert_eq!(valid.global.animations.to_animations().fps, 1000);
        RuntimeConfig::parse(
            "global:\n  animations:\n    enabled: true\n    fps: 1\n    active:\n      - type: Fade\n        duration: 3.4028235e38\n",
        )
        .unwrap();

        for (fps, duration, expected) in [
            ("0", "200", "global.animations.fps"),
            ("1001", "200", "global.animations.fps"),
            ("60.5", "200", "global.animations.fps"),
            ("not-a-number", "200", "global.animations.fps"),
            ("60", "0", "global.animations.active[1].duration"),
            ("60", ".nan", "global.animations.active[1].duration"),
            ("60", ".inf", "global.animations.active[1].duration"),
            (
                "60",
                "not-a-duration",
                "global.animations.active[1].duration",
            ),
        ] {
            let yaml = format!(
                "global:\n  animations:\n    enabled: true\n    fps: {fps}\n    active:\n      - type: Fade\n        duration: {duration}\n"
            );
            let error = RuntimeConfig::parse(&yaml).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "error {error:#} did not mention {expected}"
            );
        }
    }

    #[test]
    fn disabled_animation_timing_is_ignored_for_global_and_window_rules() {
        let runtime = RuntimeConfig::parse(
            "global:\n  animations:\n    enabled: false\n    fps: 0\n    active:\n      - type: Fade\n        duration: .nan\nwindow_rules:\n  - match: Title\n    name: example\n    animations:\n      enabled: false\n      fps: 1001\n      active:\n        - type: Spiral\n          duration: 0\n",
        )
        .unwrap();

        assert!(runtime.global.animations.to_animations().active.is_empty());
        assert!(
            runtime.window_rules()[0]
                .source()
                .animations
                .as_ref()
                .unwrap()
                .to_animations()
                .active
                .is_empty()
        );

        RuntimeConfig::parse(
            "global:\n  animations:\n    enabled: false\n    fps: not-a-number\n    active:\n      - type: Fade\n        duration: not-a-duration\n",
        )
        .unwrap();
    }

    #[test]
    fn enabled_window_rule_animation_timing_is_validated_with_context() {
        let error = RuntimeConfig::parse(
            "window_rules:\n  - match: Title\n    name: example\n    animations:\n      enabled: true\n      fps: 60\n      active:\n        - type: Fade\n          duration: 0\n",
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("window_rules[1].animations.active[1].duration"));
    }

    #[test]
    #[serial_test::serial]
    fn invalid_update_preserves_active_runtime_config() {
        let before = APP_STATE.config.read().unwrap().clone();
        let generation = APP_STATE.config_generation();
        let result = APP_STATE.update_config(|source| {
            source.window_rules.push(rule(
                Some(MatchKind::Title),
                Some("["),
                Some(MatchStrategy::Regex),
            ));
        });

        assert!(result.is_err());
        assert_eq!(*APP_STATE.config.read().unwrap(), before);
        assert_eq!(APP_STATE.config_generation(), generation);

        APP_STATE
            .update_config(|source| source.enable_logging = !source.enable_logging)
            .unwrap();
        assert_eq!(APP_STATE.config_generation(), generation + 1);

        APP_STATE
            .update_config(|source| source.enable_logging = before.source().enable_logging)
            .unwrap();
        assert_eq!(APP_STATE.config_generation(), generation + 2);
    }
}

#[cfg(test)]
mod reload_plan_tests {
    use super::*;

    #[test]
    fn renderer_plan_prepares_only_a_missing_required_renderer() {
        let install = prepare_renderer_plan(true, false, || Ok("new renderer")).unwrap();
        assert!(matches!(install, RendererPlan::Install("new renderer")));

        let keep: RendererPlan<()> = prepare_renderer_plan(true, true, || {
            anyhow::bail!("an active renderer must be reused")
        })
        .unwrap();
        assert!(matches!(keep, RendererPlan::Keep));

        let remove: RendererPlan<()> = prepare_renderer_plan(false, true, || {
            anyhow::bail!("a stopping renderer must not be recreated")
        })
        .unwrap();
        assert!(matches!(remove, RendererPlan::Remove));
    }

    #[test]
    fn renderer_preparation_failure_is_returned_without_a_plan() {
        let result: anyhow::Result<RendererPlan<()>> =
            prepare_renderer_plan(true, false, || anyhow::bail!("injected renderer failure"));

        assert!(result.is_err());
    }
}

#[cfg(test)]
mod directory_notification_tests {
    use super::*;

    fn record(name: &str) -> Vec<u8> {
        let units = name.encode_utf16().collect::<Vec<_>>();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&3u32.to_ne_bytes());
        bytes.extend_from_slice(&(units.len() as u32 * 2).to_ne_bytes());
        for unit in units {
            bytes.extend_from_slice(&unit.to_ne_bytes());
        }
        bytes
    }

    fn set_field(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn batch(names: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let mut entry = record(name);
            if index + 1 < names.len() {
                entry.resize(entry.len().next_multiple_of(4), 0);
                let next = entry.len() as u32;
                set_field(&mut entry, 0, next);
            }
            bytes.extend(entry);
        }
        bytes
    }

    fn process(bytes: &[u8], returned: u32) -> (anyhow::Result<()>, usize) {
        let mut callbacks = 0;
        let result =
            ConfigWatcher::process_dir_change_notifs(bytes, returned, "config.yaml", || {
                callbacks += 1
            });
        (result, callbacks)
    }

    #[test]
    fn valid_batches_match_once_regardless_of_position_or_action() {
        for names in [
            vec!["other.yaml"],
            vec!["config.yaml"],
            vec!["config.yaml", "other.yaml", "config.yaml"],
            vec!["other.yaml", "config.yaml", "another.yaml"],
            vec!["other.yaml", "another.yaml", "config.yaml"],
        ] {
            let bytes = batch(&names);
            let (result, callbacks) = process(&bytes, bytes.len() as u32);
            result.unwrap();
            assert_eq!(callbacks, usize::from(names.contains(&"config.yaml")));
        }

        let mut bytes = record("config.yaml");
        set_field(&mut bytes, 4, u32::MAX);
        assert!(notification_contains_config(&bytes, "config.yaml").unwrap());
        assert!(notification_contains_config(&record(""), "").unwrap());
        assert!(notification_contains_config(&record("配置-😀.yaml"), "配置-😀.yaml").unwrap());
    }

    #[test]
    fn unaligned_byte_slice_and_terminal_padding_are_supported() {
        let mut bytes = vec![0];
        bytes.extend(record("config.yaml"));
        let end = bytes.len();
        assert!(notification_contains_config(&bytes[1..], "config.yaml").unwrap());
        bytes.resize(1 + (end - 1).next_multiple_of(4), 0xAB);
        assert!(notification_contains_config(&bytes[1..], "config.yaml").unwrap());
    }

    #[test]
    fn zero_returned_bytes_resynchronize_config_after_lost_notifications() {
        let (result, callbacks) = process(&[], 0);
        result.unwrap();
        assert_eq!(callbacks, 1);
    }

    #[test]
    fn returned_byte_count_limits_the_initialized_range() {
        let entry = record("config.yaml");
        let mut buffer = [0u8; 1024];
        buffer[..entry.len()].copy_from_slice(&entry);
        buffer[entry.len()..].fill(0xFF);
        let (result, callbacks) = process(&buffer, entry.len() as u32);
        result.unwrap();
        assert_eq!(callbacks, 1);
        for count in [1025, u32::MAX] {
            let (result, callbacks) = process(&buffer, count);
            assert!(format!("{:#}", result.unwrap_err()).contains("exceeds buffer"));
            assert_eq!(callbacks, 0);
        }
    }

    #[test]
    fn every_truncated_header_is_rejected_without_callback() {
        for length in 1..FILE_NOTIFY_HEADER_BYTES {
            let bytes = vec![0; length];
            let (result, callbacks) = process(&bytes, length as u32);
            assert!(format!("{:#}", result.unwrap_err()).contains("truncated header"));
            assert_eq!(callbacks, 0);
        }
    }

    #[test]
    fn malformed_names_offsets_and_trailing_data_are_rejected() {
        let mut cases = Vec::new();
        for length in [1, 3, 24, u32::MAX - 1, u32::MAX] {
            let mut bytes = record("config.yaml");
            set_field(&mut bytes, 8, length);
            cases.push(bytes);
        }
        let mut truncated = record("config.yaml");
        truncated.pop();
        cases.push(truncated);

        for next in [
            1,
            4,
            8,
            12,
            13,
            16,
            32,
            34,
            36,
            1024,
            u32::MAX - 3,
            u32::MAX,
        ] {
            let mut bytes = record("config.yaml");
            set_field(&mut bytes, 0, next);
            cases.push(bytes);
        }
        let mut invalid_utf16 = record("a");
        invalid_utf16[12..14].copy_from_slice(&0xD800u16.to_ne_bytes());
        cases.push(invalid_utf16);

        let mut excess_padding = record("config.yaml");
        excess_padding.resize(excess_padding.len().next_multiple_of(4) + 1, 0);
        cases.push(excess_padding);
        for bytes in cases {
            let (result, callbacks) = process(&bytes, bytes.len() as u32);
            assert!(result.is_err(), "accepted malformed bytes: {bytes:?}");
            assert_eq!(callbacks, 0);
        }
    }

    #[test]
    fn matching_record_does_not_hide_malformed_later_records() {
        let valid = batch(&["config.yaml", "other.yaml"]);
        let second = u32::from_ne_bytes(valid[..4].try_into().unwrap()) as usize;
        for length in 0..FILE_NOTIFY_HEADER_BYTES {
            let bytes = &valid[..second + length];
            let (result, callbacks) = process(bytes, bytes.len() as u32);
            assert!(result.is_err());
            assert_eq!(callbacks, 0);
        }
        let mut malformed = valid.clone();
        set_field(&mut malformed, second + 8, u32::MAX);
        let (result, callbacks) = process(&malformed, malformed.len() as u32);
        assert!(result.is_err());
        assert_eq!(callbacks, 0);

        let (result, callbacks) = process(&valid, valid.len() as u32);
        result.unwrap();
        assert_eq!(callbacks, 1);
    }

    #[test]
    fn offset_arithmetic_rejects_overflow() {
        for field in ["header length", "FileNameLength", "NextEntryOffset"] {
            assert!(notification_end(usize::MAX, 1, field).is_err());
            assert!(notification_end(1, usize::MAX, field).is_err());
            assert_eq!(notification_end(0, 12, field).unwrap(), 12);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_or_invoke_callback_on_error() {
        let mut seed = 0x12345678u32;
        for length in 0..=1024 {
            let mut bytes = vec![0; length];
            for byte in &mut bytes {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = (seed >> 24) as u8;
            }
            let (result, callbacks) = process(&bytes, length as u32);
            if result.is_err() {
                assert_eq!(callbacks, 0);
            }
        }
    }

    #[test]
    fn native_output_buffer_is_dword_aligned() {
        assert!(std::mem::align_of::<DirectoryNotificationBuffer>() >= 4);
        let buffer = DirectoryNotificationBuffer([0; 1024]);
        assert_eq!(buffer.0.as_ptr() as usize % 4, 0);
    }
}

#[cfg(test)]
mod config_watcher_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    static CONFIG_RELOAD_TEST: Mutex<()> = Mutex::new(());
    static CONFIG_WATCHER_TEST: Mutex<()> = Mutex::new(());
    static CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
    static CALLBACK_STARTED: AtomicBool = AtomicBool::new(false);
    static CALLBACK_RELEASED: AtomicBool = AtomicBool::new(false);

    fn unused_callback() {}

    fn counting_callback() {
        CALLBACK_COUNT.fetch_add(1, Ordering::Release);
    }

    fn blocking_callback() {
        CALLBACK_STARTED.store(true, Ordering::Release);
        while !CALLBACK_RELEASED.load(Ordering::Acquire) {
            thread::park_timeout(time::Duration::from_millis(5));
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = time::Instant::now() + time::Duration::from_secs(2);
        while !condition() && time::Instant::now() < deadline {
            thread::sleep(time::Duration::from_millis(5));
        }
        assert!(condition());
    }

    fn test_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "tacky-borders-config-watcher-{name}-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ))
    }

    #[test]
    fn blocked_watcher_drop_cancels_and_joins_promptly() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        let directory = test_directory("blocked");
        fs::create_dir_all(&directory).unwrap();
        let watcher =
            ConfigWatcher::new(directory.join("config.yaml"), 60_000, unused_callback).unwrap();

        let started = time::Instant::now();
        drop(watcher);

        assert!(started.elapsed() < time::Duration::from_secs(2));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn debounce_wait_is_unparked_during_drop() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        CALLBACK_COUNT.store(0, Ordering::Release);
        let directory = test_directory("debounce");
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("config.yaml");
        let watcher = ConfigWatcher::new(config_path.clone(), 60_000, counting_callback).unwrap();

        fs::write(config_path, "watch_config_changes: true\n").unwrap();
        wait_until(|| CALLBACK_COUNT.load(Ordering::Acquire) == 1);

        let started = time::Instant::now();
        drop(watcher);

        assert!(started.elapsed() < time::Duration::from_secs(2));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_config_replacement_triggers_callback() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        CALLBACK_COUNT.store(0, Ordering::Release);
        let directory = test_directory("atomic-replacement");
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("config.yaml");
        let replacement_path = directory.join("config.yaml.tmp");
        fs::write(&config_path, "watch_config_changes: true\n").unwrap();
        let watcher = ConfigWatcher::new(config_path.clone(), 10, counting_callback).unwrap();

        fs::write(&replacement_path, "watch_config_changes: false\n").unwrap();
        fs::rename(&replacement_path, &config_path).unwrap();
        wait_until(|| CALLBACK_COUNT.load(Ordering::Acquire) > 0);

        drop(watcher);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn callback_active_drop_waits_for_callback_then_joins() {
        let _guard = CONFIG_WATCHER_TEST.lock().unwrap();
        CALLBACK_STARTED.store(false, Ordering::Release);
        CALLBACK_RELEASED.store(false, Ordering::Release);
        let directory = test_directory("callback");
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("config.yaml");
        let watcher = ConfigWatcher::new(config_path.clone(), 60_000, blocking_callback).unwrap();

        fs::write(config_path, "watch_config_changes: true\n").unwrap();
        wait_until(|| CALLBACK_STARTED.load(Ordering::Acquire));

        let release_thread = thread::spawn(|| {
            thread::sleep(time::Duration::from_millis(50));
            CALLBACK_RELEASED.store(true, Ordering::Release);
        });

        let started = time::Instant::now();
        drop(watcher);
        let elapsed = started.elapsed();
        release_thread.join().unwrap();

        assert!(elapsed >= time::Duration::from_millis(40));
        assert!(elapsed < time::Duration::from_secs(2));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn operation_not_found_is_expected_cancellation_race() {
        let expected = windows::core::Error::from_hresult(ERROR_NOT_FOUND.to_hresult());
        let unexpected = windows::core::Error::from_hresult(
            windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult(),
        );

        assert!(is_operation_not_found(&expected));
        assert!(!is_operation_not_found(&unexpected));
    }

    #[test]
    fn read_cancellation_is_expected_only_after_stop_is_requested() {
        let aborted = windows::core::Error::from_hresult(ERROR_OPERATION_ABORTED.to_hresult());

        assert!(is_expected_watcher_cancellation(&aborted, true));
        assert!(!is_expected_watcher_cancellation(&aborted, false));
    }

    #[test]
    fn other_read_failures_remain_unexpected_even_during_shutdown() {
        for code in [
            windows::Win32::Foundation::ERROR_ACCESS_DENIED,
            windows::Win32::Foundation::ERROR_INVALID_HANDLE,
            ERROR_NOT_FOUND,
        ] {
            let error = windows::core::Error::from_hresult(code.to_hresult());
            for stopping in [false, true] {
                assert!(
                    !is_expected_watcher_cancellation(&error, stopping),
                    "{code:?} must remain an error when stopping={stopping}"
                );
            }
        }
    }

    #[test]
    fn current_thread_join_is_deferred_to_reaper() {
        let (handle_sender, handle_receiver) = std::sync::mpsc::sync_channel(1);
        let returned = Arc::new(AtomicBool::new(false));
        let worker_returned = returned.clone();
        let worker = thread::spawn(move || {
            let own_handle = handle_receiver.recv().unwrap();
            join_or_reap_config_watcher(own_handle, false);
            worker_returned.store(true, Ordering::Release);
        });

        handle_sender.send(worker).unwrap();
        wait_until(|| returned.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_failure_defers_join_without_blocking_owner() {
        let release = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_release = release.clone();
        let worker_exited = exited.clone();
        let worker = thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                thread::park_timeout(time::Duration::from_millis(5));
            }
            worker_exited.store(true, Ordering::Release);
        });

        let started = time::Instant::now();
        join_or_reap_config_watcher(worker, true);
        assert!(started.elapsed() < time::Duration::from_secs(1));

        release.store(true, Ordering::Release);
        wait_until(|| exited.load(Ordering::Acquire));
    }

    #[test]
    fn worker_panic_is_consumed_by_join_logging() {
        let worker = thread::spawn(|| panic!("config watcher test panic"));

        join_or_reap_config_watcher(worker, false);
    }

    #[test]
    fn config_reload_requests_coalesce_until_consumed() {
        let _guard = CONFIG_RELOAD_TEST.lock().unwrap();
        begin_config_reload();
        let submissions = AtomicUsize::new(0);

        request_config_reload_with(|| {
            submissions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        request_config_reload_with(|| {
            submissions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert_eq!(submissions.load(Ordering::Relaxed), 1);

        begin_config_reload();
        request_config_reload_with(|| {
            submissions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert_eq!(submissions.load(Ordering::Relaxed), 2);
        begin_config_reload();
    }

    #[test]
    fn rejected_config_reload_request_can_retry() {
        let _guard = CONFIG_RELOAD_TEST.lock().unwrap();
        begin_config_reload();

        assert!(request_config_reload_with(|| Err(anyhow!("rejected"))).is_err());
        let mut submitted = false;
        request_config_reload_with(|| {
            submitted = true;
            Ok(())
        })
        .unwrap();

        assert!(submitted);
        begin_config_reload();
    }
}
