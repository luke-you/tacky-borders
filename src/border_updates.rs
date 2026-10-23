use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use crate::border_registry::{BorderControl, BorderEvent};
use crate::colors::ColorBrushConfig;
use crate::config::{OffsetConfig, RadiusConfig, WidthConfig};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AppearanceCommandId(pub u64);

#[derive(Debug, Default)]
pub struct AppearanceSequencer {
    next_command: u64,
    pub config_revision: u64,
    pub global_stamps: AppearanceStamps,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AppearanceStamps {
    pub active: AppearanceCommandId,
    pub inactive: AppearanceCommandId,
    pub width: AppearanceCommandId,
    pub offset: AppearanceCommandId,
    pub radius: AppearanceCommandId,
}

impl AppearanceSequencer {
    pub fn next_command(&mut self) -> Option<AppearanceCommandId> {
        self.next_command = self.next_command.checked_add(1)?;
        Some(AppearanceCommandId(self.next_command))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StampedUpdate<Value> {
    pub command_id: AppearanceCommandId,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum IpcUpdate {
    Colors {
        active: Option<ColorBrushConfig>,
        inactive: Option<ColorBrushConfig>,
    },
    Width(WidthConfig),
    Offset(OffsetConfig),
    Radius(RadiusConfig),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PendingColors {
    pub active: Option<StampedUpdate<ColorBrushConfig>>,
    pub inactive: Option<StampedUpdate<ColorBrushConfig>>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PendingIpcUpdates {
    pub colors: Option<PendingColors>,
    pub width: Option<StampedUpdate<WidthConfig>>,
    pub offset: Option<StampedUpdate<OffsetConfig>>,
    pub radius: Option<StampedUpdate<RadiusConfig>>,
}

fn merge_slot<Value>(
    slot: &mut Option<StampedUpdate<Value>>,
    incoming: Option<StampedUpdate<Value>>,
) {
    if let Some(incoming) = incoming
        && slot
            .as_ref()
            .is_none_or(|current| incoming.command_id >= current.command_id)
    {
        *slot = Some(incoming);
    }
}

impl PendingIpcUpdates {
    pub fn from_config(
        config: &crate::border_config::BorderConfig,
        stamps: AppearanceStamps,
    ) -> Self {
        Self {
            colors: Some(PendingColors {
                active: Some(StampedUpdate {
                    command_id: stamps.active,
                    value: config.active_color.clone(),
                }),
                inactive: Some(StampedUpdate {
                    command_id: stamps.inactive,
                    value: config.inactive_color.clone(),
                }),
            }),
            width: Some(StampedUpdate {
                command_id: stamps.width,
                value: config.width,
            }),
            offset: Some(StampedUpdate {
                command_id: stamps.offset,
                value: config.offset,
            }),
            radius: Some(StampedUpdate {
                command_id: stamps.radius,
                value: config.radius,
            }),
        }
    }

    pub fn apply_to_config(&self, config: &mut crate::border_config::BorderConfig) {
        if let Some(colors) = &self.colors {
            if let Some(active) = &colors.active {
                config.active_color = active.value.clone();
            }
            if let Some(inactive) = &colors.inactive {
                config.inactive_color = inactive.value.clone();
            }
        }
        if let Some(width) = &self.width {
            config.width = width.value;
        }
        if let Some(offset) = &self.offset {
            config.offset = offset.value;
        }
        if let Some(radius) = &self.radius {
            config.radius = radius.value;
        }
    }

    pub fn merge(&mut self, incoming: Self) {
        if let Some(colors) = incoming.colors {
            let current = self.colors.get_or_insert_with(PendingColors::default);
            merge_slot(&mut current.active, colors.active);
            merge_slot(&mut current.inactive, colors.inactive);
        }
        merge_slot(&mut self.width, incoming.width);
        merge_slot(&mut self.offset, incoming.offset);
        merge_slot(&mut self.radius, incoming.radius);
    }

    pub fn from_update(command_id: AppearanceCommandId, update: IpcUpdate) -> Self {
        let mut pending = Self::default();
        match update {
            IpcUpdate::Colors { active, inactive } => {
                pending.colors = Some(PendingColors {
                    active: active.map(|value| StampedUpdate { command_id, value }),
                    inactive: inactive.map(|value| StampedUpdate { command_id, value }),
                });
            }
            IpcUpdate::Width(value) => pending.width = Some(StampedUpdate { command_id, value }),
            IpcUpdate::Offset(value) => pending.offset = Some(StampedUpdate { command_id, value }),
            IpcUpdate::Radius(value) => pending.radius = Some(StampedUpdate { command_id, value }),
        }
        pending
    }

    pub fn is_empty(&self) -> bool {
        self.colors
            .as_ref()
            .is_none_or(|colors| colors.active.is_none() && colors.inactive.is_none())
            && self.width.is_none()
            && self.offset.is_none()
            && self.radius.is_none()
    }
}

#[derive(Debug)]
struct MailboxState {
    accepting: bool,
    pending: PendingIpcUpdates,
}

#[derive(Debug)]
pub struct BorderUpdateMailbox {
    state: Mutex<MailboxState>,
    closed: AtomicBool,
    pub wake_event: BorderEvent,
    control: Arc<BorderControl>,
}

pub(crate) struct MailboxAdmission<'a> {
    mailbox: &'a BorderUpdateMailbox,
    state: MutexGuard<'a, MailboxState>,
}

impl MailboxAdmission<'_> {
    pub(crate) fn enqueue(
        self,
        command_id: AppearanceCommandId,
        update: IpcUpdate,
    ) -> Result<(), EnqueueError> {
        self.mailbox
            .enqueue_locked(self.state, command_id, update, || {
                self.mailbox.wake_event.signal()
            })
    }
}

#[derive(Debug)]
pub enum EnqueueError {
    Closed,
    Poisoned,
    Wake(windows::core::Error),
}

#[derive(Debug)]
pub enum MailboxDrain {
    Updates(PendingIpcUpdates),
    Deferred,
    Failed,
}

#[derive(Debug)]
pub enum MailboxClosure {
    Closed {
        reclaimed: PendingIpcUpdates,
        poisoned: bool,
    },
    Deferred,
}

impl BorderUpdateMailbox {
    pub fn new(control: Arc<BorderControl>) -> windows::core::Result<Self> {
        Ok(Self {
            state: Mutex::new(MailboxState {
                accepting: true,
                pending: PendingIpcUpdates::default(),
            }),
            closed: AtomicBool::new(false),
            wake_event: BorderEvent::new(false)?,
            control,
        })
    }

    pub fn control(&self) -> &Arc<BorderControl> {
        &self.control
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn hint(&self, bits: u8) -> anyhow::Result<()> {
        if self.control.is_stopped() || self.is_closed() {
            anyhow::bail!("border stopped");
        }
        self.control.request_work(bits);
        self.wake_event.signal()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn while_locked(&self, action: impl FnOnce()) {
        let _guard = self.state.lock().unwrap();
        action();
    }

    pub fn enqueue(
        &self,
        command_id: AppearanceCommandId,
        update: IpcUpdate,
    ) -> Result<(), EnqueueError> {
        self.enqueue_with(command_id, update, || self.wake_event.signal())
    }

    pub(crate) fn admit(
        &self,
        context: &crate::command_context::CommandContext,
    ) -> anyhow::Result<Option<MailboxAdmission<'_>>> {
        loop {
            context.check()?;
            match self.state.try_lock() {
                Ok(state) => {
                    return Ok(Some(MailboxAdmission {
                        mailbox: self,
                        state,
                    }));
                }
                Err(TryLockError::WouldBlock) => context.pause()?,
                Err(TryLockError::Poisoned(poison)) => {
                    let reclaimed = self.reclaim(poison.into_inner());
                    self.control.record_stop();
                    let _ = self.control.signal_stop();
                    drop(reclaimed);
                    return Ok(None);
                }
            }
        }
    }

    fn enqueue_with(
        &self,
        command_id: AppearanceCommandId,
        update: IpcUpdate,
        signal: impl FnOnce() -> windows::core::Result<()>,
    ) -> Result<(), EnqueueError> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poison) => {
                let reclaimed = self.reclaim(poison.into_inner());
                self.control.record_stop();
                let _ = self.control.signal_stop();
                drop(reclaimed);
                return Err(EnqueueError::Poisoned);
            }
        };
        self.enqueue_locked(state, command_id, update, signal)
    }

    fn enqueue_locked(
        &self,
        mut state: MutexGuard<'_, MailboxState>,
        command_id: AppearanceCommandId,
        update: IpcUpdate,
        signal: impl FnOnce() -> windows::core::Result<()>,
    ) -> Result<(), EnqueueError> {
        if !state.accepting || self.control.is_stopped() {
            return Err(EnqueueError::Closed);
        }
        let previous = state.pending.clone();
        state
            .pending
            .merge(PendingIpcUpdates::from_update(command_id, update));
        if let Err(error) = signal() {
            let rejected = std::mem::replace(&mut state.pending, previous);
            drop(state);
            drop(rejected);
            return Err(EnqueueError::Wake(error));
        }
        Ok(())
    }

    pub fn try_drain(&self) -> MailboxDrain {
        match self.state.try_lock() {
            Ok(mut state) => {
                if self.control.is_stopped() {
                    return MailboxDrain::Failed;
                }
                let pending = std::mem::take(&mut state.pending);
                drop(state);
                MailboxDrain::Updates(pending)
            }
            Err(TryLockError::WouldBlock) => MailboxDrain::Deferred,
            Err(TryLockError::Poisoned(poison)) => {
                let reclaimed = self.reclaim(poison.into_inner());
                self.control.record_stop();
                let _ = self.control.signal_stop();
                drop(reclaimed);
                MailboxDrain::Failed
            }
        }
    }

    pub fn try_close(&self) -> MailboxClosure {
        let (state, poisoned) = match self.state.try_lock() {
            Ok(guard) => (guard, false),
            Err(TryLockError::WouldBlock) => return MailboxClosure::Deferred,
            Err(TryLockError::Poisoned(poison)) => (poison.into_inner(), true),
        };
        let reclaimed = self.reclaim(state);
        if poisoned {
            self.control.record_stop();
        }
        MailboxClosure::Closed {
            reclaimed,
            poisoned,
        }
    }

    fn reclaim(&self, mut state: std::sync::MutexGuard<'_, MailboxState>) -> PendingIpcUpdates {
        state.accepting = false;
        let reclaimed = std::mem::take(&mut state.pending);
        self.closed.store(true, Ordering::Release);
        drop(state);
        reclaimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn mailbox() -> Arc<BorderUpdateMailbox> {
        Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new().unwrap())).unwrap())
    }

    fn drain(mailbox: &BorderUpdateMailbox) -> PendingIpcUpdates {
        match mailbox.try_drain() {
            MailboxDrain::Updates(updates) => updates,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn independent_colors_and_latest_component_stamps() {
        let mailbox = mailbox();
        mailbox
            .enqueue(
                AppearanceCommandId(2),
                IpcUpdate::Colors {
                    active: None,
                    inactive: Some(ColorBrushConfig::Solid("red".into())),
                },
            )
            .unwrap();
        mailbox
            .enqueue(
                AppearanceCommandId(3),
                IpcUpdate::Colors {
                    active: Some(ColorBrushConfig::Solid("green".into())),
                    inactive: None,
                },
            )
            .unwrap();
        mailbox
            .enqueue(
                AppearanceCommandId(1),
                IpcUpdate::Colors {
                    active: Some(ColorBrushConfig::Solid("old".into())),
                    inactive: None,
                },
            )
            .unwrap();
        let colors = drain(&mailbox).colors.unwrap();
        assert_eq!(
            colors.active.unwrap().value,
            ColorBrushConfig::Solid("green".into())
        );
        assert_eq!(
            colors.inactive.unwrap().value,
            ColorBrushConfig::Solid("red".into())
        );
        assert!(drain(&mailbox).is_empty());
    }

    #[test]
    fn four_slots_remain_bounded_under_burst() {
        let mailbox = mailbox();
        for command in 1..1000 {
            mailbox
                .enqueue(
                    AppearanceCommandId(command),
                    IpcUpdate::Width(WidthConfig::new(command as f32)),
                )
                .unwrap();
            mailbox
                .enqueue(
                    AppearanceCommandId(command),
                    IpcUpdate::Offset(OffsetConfig::new(command as i32)),
                )
                .unwrap();
            mailbox
                .enqueue(
                    AppearanceCommandId(command),
                    IpcUpdate::Radius(RadiusConfig::Custom(command as f32)),
                )
                .unwrap();
        }
        let pending = drain(&mailbox);
        assert_eq!(pending.width.unwrap().command_id, AppearanceCommandId(999));
        assert_eq!(pending.offset.unwrap().command_id, AppearanceCommandId(999));
        assert_eq!(pending.radius.unwrap().command_id, AppearanceCommandId(999));
    }

    #[test]
    fn wake_failure_rolls_back_every_component() {
        let mailbox = mailbox();
        mailbox
            .enqueue(
                AppearanceCommandId(1),
                IpcUpdate::Width(WidthConfig::new(4.0)),
            )
            .unwrap();
        let result = mailbox.enqueue_with(
            AppearanceCommandId(2),
            IpcUpdate::Width(WidthConfig::new(8.0)),
            || {
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80004005u32 as i32,
                )))
            },
        );
        assert!(matches!(result, Err(EnqueueError::Wake(_))));
        assert_eq!(drain(&mailbox).width.unwrap().value, WidthConfig::new(4.0));
    }

    #[test]
    fn consumed_wake_and_busy_drain_preserve_values_and_stop() {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;
        let mailbox = mailbox();
        mailbox
            .enqueue(
                AppearanceCommandId(1),
                IpcUpdate::Width(WidthConfig::new(4.0)),
            )
            .unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(mailbox.wake_event.handle(), 0) },
            WAIT_OBJECT_0
        );
        assert_eq!(
            unsafe { WaitForSingleObject(mailbox.wake_event.handle(), 0) },
            WAIT_TIMEOUT
        );
        let guard = mailbox.state.lock().unwrap();
        assert!(matches!(mailbox.try_drain(), MailboxDrain::Deferred));
        assert!(matches!(mailbox.try_close(), MailboxClosure::Deferred));
        mailbox.control.record_stop();
        assert!(mailbox.control.is_stopped());
        drop(guard);
        assert!(matches!(
            mailbox.enqueue(
                AppearanceCommandId(2),
                IpcUpdate::Width(WidthConfig::new(8.0))
            ),
            Err(EnqueueError::Closed)
        ));
        match mailbox.try_close() {
            MailboxClosure::Closed {
                reclaimed,
                poisoned,
            } => {
                assert!(!poisoned);
                assert!(reclaimed.width.is_some());
            }
            _ => panic!(),
        }
        match mailbox.try_close() {
            MailboxClosure::Closed { reclaimed, .. } => assert!(reclaimed.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn deferred_drain_succeeds_without_another_enqueue() {
        let mailbox = mailbox();
        mailbox
            .enqueue(
                AppearanceCommandId(1),
                IpcUpdate::Width(WidthConfig::new(4.0)),
            )
            .unwrap();
        let guard = mailbox.state.lock().unwrap();
        assert!(matches!(mailbox.try_drain(), MailboxDrain::Deferred));
        drop(guard);
        assert!(drain(&mailbox).width.is_some());
    }

    #[test]
    fn poison_stops_admission_and_reclaims_pending_once() {
        let mailbox = mailbox();
        mailbox
            .enqueue(
                AppearanceCommandId(1),
                IpcUpdate::Width(WidthConfig::new(4.0)),
            )
            .unwrap();
        let poisoned = mailbox.clone();
        assert!(
            thread::spawn(move || {
                let _guard = poisoned.state.lock().unwrap();
                panic!("injected poison");
            })
            .join()
            .is_err()
        );
        assert!(matches!(mailbox.try_drain(), MailboxDrain::Failed));
        assert!(mailbox.control.is_stopped());
        assert!(mailbox.is_closed());
        assert!(
            mailbox
                .state
                .lock()
                .unwrap_err()
                .into_inner()
                .pending
                .is_empty()
        );
        match mailbox.try_close() {
            MailboxClosure::Closed {
                reclaimed,
                poisoned,
            } => {
                assert!(poisoned);
                assert!(reclaimed.is_empty());
            }
            _ => panic!(),
        }
        match mailbox.try_close() {
            MailboxClosure::Closed { reclaimed, .. } => assert!(reclaimed.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn command_exhaustion_does_not_wrap() {
        let mut sequencer = AppearanceSequencer {
            next_command: u64::MAX,
            ..Default::default()
        };
        assert!(sequencer.next_command().is_none());
        assert!(sequencer.next_command().is_none());
    }

    #[test]
    fn poisoned_enqueue_physically_closes_before_returning_failure() {
        let mailbox = mailbox();
        mailbox
            .enqueue(
                AppearanceCommandId(1),
                IpcUpdate::Width(WidthConfig::new(4.0)),
            )
            .unwrap();
        let poisoned = mailbox.clone();
        assert!(
            thread::spawn(move || {
                let _guard = poisoned.state.lock().unwrap();
                panic!("injected poison");
            })
            .join()
            .is_err()
        );
        assert!(matches!(
            mailbox.enqueue(
                AppearanceCommandId(2),
                IpcUpdate::Width(WidthConfig::new(8.0))
            ),
            Err(EnqueueError::Poisoned)
        ));
        assert!(mailbox.is_closed());
        assert!(mailbox.control.is_stopped());
        assert!(
            mailbox
                .state
                .lock()
                .unwrap_err()
                .into_inner()
                .pending
                .is_empty()
        );
    }
}
