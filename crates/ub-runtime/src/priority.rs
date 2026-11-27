use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, IDLE_PRIORITY_CLASS, SetPriorityClass,
};

const ENV_PRIORITY: &str = "UB_TOOLS_PRIORITY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessPriority {
    Normal,
    BelowNormal,
    Idle,
}

pub(crate) fn parse_priority(raw: Option<&str>) -> ProcessPriority {
    let Some(value) = raw else {
        return ProcessPriority::BelowNormal;
    };
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("normal") {
        ProcessPriority::Normal
    } else if trimmed.eq_ignore_ascii_case("idle") {
        ProcessPriority::Idle
    } else {
        // Default, "below_normal", "low", and unknown values all map to below-normal priority.
        ProcessPriority::BelowNormal
    }
}

pub(crate) fn current_priority() -> ProcessPriority {
    let raw = std::env::var(ENV_PRIORITY).ok();
    parse_priority(raw.as_deref())
}

pub(crate) fn child_creation_flags_for(base_flags: u32, priority: ProcessPriority) -> u32 {
    #[cfg(windows)]
    {
        match priority {
            ProcessPriority::Normal => base_flags,
            ProcessPriority::BelowNormal => base_flags | BELOW_NORMAL_PRIORITY_CLASS,
            ProcessPriority::Idle => base_flags | IDLE_PRIORITY_CLASS,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = priority;
        base_flags
    }
}

/// Returns Windows child-process creation flags with configured priority ORed in.
///
/// On Windows, ORs `BELOW_NORMAL_PRIORITY_CLASS` (or `IDLE_PRIORITY_CLASS` when
/// `UB_TOOLS_PRIORITY=idle`) onto `base_flags` unless `UB_TOOLS_PRIORITY=normal`.
/// Returns `base_flags` unchanged on non-Windows targets.
pub fn child_creation_flags(base_flags: u32) -> u32 {
    child_creation_flags_for(base_flags, current_priority())
}

/// Applies the configured child-process priority creation flags to `cmd`.
///
/// On Windows, sets `CommandExt::creation_flags(child_creation_flags(0))`.
/// Safe no-op on non-Windows targets.
pub fn apply_child_priority(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(child_creation_flags(0));
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_current_process_priority_class(priority_class: u32) {
    // SAFETY: `GetCurrentProcess` returns the pseudo-handle for the calling process,
    // and `SetPriorityClass` only reads that pseudo-handle and a Win32 priority class constant.
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), priority_class);
    }
}

pub(crate) fn apply_current_process_priority() {
    #[cfg(windows)]
    {
        match current_priority() {
            ProcessPriority::Normal => {}
            ProcessPriority::BelowNormal => {
                set_current_process_priority_class(BELOW_NORMAL_PRIORITY_CLASS);
            }
            ProcessPriority::Idle => {
                set_current_process_priority_class(IDLE_PRIORITY_CLASS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_priority_variants_case_insensitively() {
        assert_eq!(parse_priority(None), ProcessPriority::BelowNormal);
        assert_eq!(parse_priority(Some("")), ProcessPriority::BelowNormal);
        assert_eq!(parse_priority(Some("   ")), ProcessPriority::BelowNormal);
        assert_eq!(
            parse_priority(Some("below_normal")),
            ProcessPriority::BelowNormal
        );
        assert_eq!(parse_priority(Some(" LOW ")), ProcessPriority::BelowNormal);
        assert_eq!(parse_priority(Some("normal")), ProcessPriority::Normal);
        assert_eq!(parse_priority(Some(" NORMAL ")), ProcessPriority::Normal);
        assert_eq!(parse_priority(Some("idle")), ProcessPriority::Idle);
        assert_eq!(parse_priority(Some(" IdLe ")), ProcessPriority::Idle);
        assert_eq!(
            parse_priority(Some("unknown")),
            ProcessPriority::BelowNormal
        );
    }

    #[test]
    fn computes_child_creation_flags_for_each_priority() {
        let base = 0x0000_0200_u32;
        #[cfg(windows)]
        {
            assert_eq!(
                child_creation_flags_for(base, ProcessPriority::Normal),
                base
            );
            assert_eq!(
                child_creation_flags_for(base, ProcessPriority::BelowNormal),
                base | BELOW_NORMAL_PRIORITY_CLASS
            );
            assert_eq!(
                child_creation_flags_for(base, ProcessPriority::Idle),
                base | IDLE_PRIORITY_CLASS
            );
            assert_eq!(
                child_creation_flags_for(0, ProcessPriority::BelowNormal),
                BELOW_NORMAL_PRIORITY_CLASS
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                child_creation_flags_for(base, ProcessPriority::Normal),
                base
            );
            assert_eq!(
                child_creation_flags_for(base, ProcessPriority::BelowNormal),
                base
            );
            assert_eq!(child_creation_flags_for(base, ProcessPriority::Idle), base);
        }
    }
}
