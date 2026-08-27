use std::time::{Duration, Instant};

/// Mode niat pengoperasian yang diminta oleh pengguna/IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentMode {
    Normal,
    Bypass,
    Disabled,
}

/// Batas waktu kedaluwarsa keamanan default untuk mode bypass via IPC (12 jam).
pub const DEFAULT_MAX_BYPASS_TTL: Duration = Duration::from_secs(12 * 3600);

/// Niat pengoperasian dengan batas waktu kedaluwarsa opsional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatingIntent {
    pub mode: IntentMode,
    pub expires_at: Option<Instant>,
}

impl Default for OperatingIntent {
    fn default() -> Self {
        Self {
            mode: IntentMode::Normal,
            expires_at: None,
        }
    }
}

impl OperatingIntent {
    pub fn normal() -> Self {
        Self::default()
    }

    pub fn disabled() -> Self {
        Self {
            mode: IntentMode::Disabled,
            expires_at: None,
        }
    }

    pub fn bypass(now: Instant, expires_in: Option<Duration>) -> Self {
        Self {
            mode: IntentMode::Bypass,
            expires_at: expires_in.map(|d| now + d),
        }
    }

    pub fn bypass_with_safety_ttl(now: Instant) -> Self {
        Self::bypass(now, Some(DEFAULT_MAX_BYPASS_TTL))
    }

    pub fn current_mode(&self, now: Instant) -> IntentMode {
        if let Some(expiry) = self.expires_at {
            if now >= expiry {
                return IntentMode::Normal;
            }
        }
        self.mode
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.expires_at
    }

    pub fn normalize(&mut self, now: Instant) {
        if let Some(expiry) = self.expires_at {
            if now >= expiry {
                *self = Self::normal();
            }
        }
    }
}
