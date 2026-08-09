Ya. Untuk `control.rs`, saya sarankan kontraknya dibuat jelas:

* **Tidak ada node ditemukan** → `Err(NoChargingNodeFound)`
* **Semua node yang ditemukan gagal ditulis** → `Err(...)`
* **Sebagian berhasil, sebagian gagal** → `Ok(ChargingWriteResult { ... })`, tetapi hasilnya secara eksplisit menunjukkan `failed > 0`
* **Semua berhasil** → `Ok(...)`
* Jangan lagi menelan error `write_sysfs()` tanpa menyimpan error terakhir.

Ada satu hal penting: `set_charging()` dipakai oleh `HardwareController`, jadi **partial failure jangan dianggap sama dengan full success**. Caller harus bisa melihat `failed > 0`.

Berikut versi yang saya rekomendasikan.

```rust
use crate::{battery::nodes::*, error::ChargerError};
use std::{fs, path::Path};

/// Write a value to a sysfs node with proper error context.
pub fn write_sysfs(path: &Path, value: &str) -> Result<(), ChargerError> {
    fs::write(path, value).map_err(|e| ChargerError::SysfsWrite {
        path: path.to_owned(),
        source: e,
    })
}

/// Result of attempting to write all charging-control nodes.
///
/// `attempted` = number of existing nodes we tried to write.
/// `succeeded` = number of successful writes.
/// `failed` = number of failed writes.
///
/// Important:
/// - `succeeded == attempted` => all writes succeeded.
/// - `succeeded > 0 && failed > 0` => partial failure.
/// - `succeeded == 0 && attempted > 0` => all writes failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargingWriteResult {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl ChargingWriteResult {
    #[inline]
    pub fn all_succeeded(&self) -> bool {
        self.attempted > 0 && self.failed == 0
    }

    #[inline]
    pub fn partial_failure(&self) -> bool {
        self.succeeded > 0 && self.failed > 0
    }

    #[inline]
    pub fn all_failed(&self) -> bool {
        self.attempted > 0 && self.succeeded == 0
    }
}

/// Enable or disable charging across all known nodes.
///
/// Semantics:
///
/// - No node exists:
///     Err(NoChargingNodeFound)
///
/// - At least one node exists and all writes succeed:
///     Ok(all_succeeded)
///
/// - At least one write succeeds but another fails:
///     Ok(partial_failure)
///
/// - Nodes exist but every write fails:
///     Err(last_write_error)
///
/// This is important because a partial write means the hardware may be in
/// a mixed state and should not be treated as a fully successful operation.
pub fn set_charging(enable: bool) -> Result<ChargingWriteResult, ChargerError> {
    let charge_val = if enable { "1" } else { "0" };
    let suspend_val = if enable { "0" } else { "1" };

    let mut result = ChargingWriteResult {
        attempted: 0,
        succeeded: 0,
        failed: 0,
    };

    let mut last_error: Option<ChargerError> = None;

    // charging_enabled-style nodes.
    for node in CHARGING_NODES {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        result.attempted += 1;

        match write_sysfs(path, charge_val) {
            Ok(()) => {
                result.succeeded += 1;
                tracing::debug!(
                    "Charging node write succeeded: {} = {}",
                    path.display(),
                    charge_val
                );
            }
            Err(e) => {
                result.failed += 1;

                tracing::warn!(
                    "Charging node write failed: {} = {}: {}",
                    path.display(),
                    charge_val,
                    e
                );

                last_error = Some(e);
            }
        }
    }

    // input_suspend-style nodes.
    for node in SUSPEND_NODES {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        result.attempted += 1;

        match write_sysfs(path, suspend_val) {
            Ok(()) => {
                result.succeeded += 1;
                tracing::debug!(
                    "Suspend node write succeeded: {} = {}",
                    path.display(),
                    suspend_val
                );
            }
            Err(e) => {
                result.failed += 1;

                tracing::warn!(
                    "Suspend node write failed: {} = {}: {}",
                    path.display(),
                    suspend_val,
                    e
                );

                last_error = Some(e);
            }
        }
    }

    // No usable charging-control node exists.
    if result.attempted == 0 {
        return Err(ChargerError::NoChargingNodeFound);
    }

    // Every existing node failed.
    //
    // Return the actual sysfs error instead of fabricating a generic error.
    if result.all_failed() {
        if let Some(error) = last_error {
            return Err(error);
        }

        // Defensive fallback. This should theoretically be unreachable because
        // all_failed() implies at least one attempted write and therefore at
        // least one error should have been recorded.
        return Err(ChargerError::SysfsWrite {
            path: Path::new("charging_nodes").to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::Other,
                "All charging node writes failed",
            ),
        });
    }

    // Either:
    //   1. all writes succeeded, or
    //   2. some succeeded and some failed.
    //
    // The caller MUST inspect `failed` / `partial_failure()`.
    if result.partial_failure() {
        tracing::warn!(
            "Charging control partially applied: {}/{} writes succeeded, {} failed",
            result.succeeded,
            result.attempted,
            result.failed
        );
    } else {
        tracing::info!(
            "Charging control applied successfully: {}/{} writes succeeded",
            result.succeeded,
            result.attempted
        );
    }

    Ok(result)
}

/// Activate bypass mode (disconnect input power from battery).
pub fn enter_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        ("/sys/class/power_supply/battery/input_suspend", "1"),
        ("/sys/class/power_supply/battery/charging_enabled", "0"),
        ("/sys/class/power_supply/main/charging_enabled", "0"),
    ];

    let mut any_found = false;
    let mut any_success = false;
    let mut last_error: Option<ChargerError> = None;

    for (path, val) in &nodes {
        let p = Path::new(path);

        if !p.exists() {
            continue;
        }

        any_found = true;

        match write_sysfs(p, val) {
            Ok(()) => {
                any_success = true;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to write bypass node {} = {}: {}",
                    path,
                    val,
                    e
                );
                last_error = Some(e);
            }
        }
    }

    if !any_found {
        Err(ChargerError::NoChargingNodeFound)
    } else if !any_success {
        last_error.unwrap_or_else(|| ChargerError::SysfsWrite {
            path: Path::new("bypass_nodes").to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::Other,
                "All bypass node writes failed",
            ),
        })
    } else {
        Ok(())
    }
}

/// Restore normal charging from bypass mode.
pub fn exit_bypass_mode() -> Result<(), ChargerError> {
    let nodes = [
        ("/sys/class/power_supply/battery/input_suspend", "0"),
        ("/sys/class/power_supply/battery/charging_enabled", "1"),
        ("/sys/class/power_supply/main/charging_enabled", "1"),
    ];

    let mut any_found = false;
    let mut any_success = false;
    let mut last_error: Option<ChargerError> = None;

    for (path, val) in &nodes {
        let p = Path::new(path);

        if !p.exists() {
            continue;
        }

        any_found = true;

        match write_sysfs(p, val) {
            Ok(()) => {
                any_success = true;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to restore bypass node {} = {}: {}",
                    path,
                    val,
                    e
                );
                last_error = Some(e);
            }
        }
    }

    if !any_found {
        Err(ChargerError::NoChargingNodeFound)
    } else if !any_success {
        last_error.unwrap_or_else(|| ChargerError::SysfsWrite {
            path: Path::new("bypass_nodes").to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::Other,
                "All bypass node writes failed",
            ),
        })
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    use std::os::unix::fs::PermissionsExt;

    let mut any_found = false;

    for node in CHARGING_NODES.iter().chain(SUSPEND_NODES.iter()) {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        any_found = true;

        match fs::metadata(path) {
            Ok(metadata) => {
                let mut perms = metadata.permissions();
                perms.set_mode(0o644);

                if let Err(e) = fs::set_permissions(path, perms) {
                    tracing::warn!(
                        "Failed to set permissions on {}: {}",
                        path.display(),
                        e
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read metadata for {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }

    if !any_found {
        Err(ChargerError::NoChargingNodeFound)
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
pub fn grant_node_permissions() -> Result<(), ChargerError> {
    Ok(())
}

/// Read the current physical hardware charging state.
///
/// Semantics:
/// - If at least one charging-control node indicates enabled => true.
/// - If nodes exist but none indicate enabled => false.
/// - If no known node exists => error.
///
/// Note:
/// This is intentionally permissive because different Android kernels expose
/// different combinations of charging_enabled and input_suspend nodes.
pub fn is_charging_enabled() -> Result<bool, ChargerError> {
    let mut any_node_found = false;
    let mut any_read_success = false;
    let mut any_enabled = false;

    // charging_enabled-style nodes:
    // 1 = charging enabled
    for node in CHARGING_NODES {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        any_node_found = true;

        match fs::read_to_string(path) {
            Ok(content) => {
                any_read_success = true;

                if content.trim() == "1" {
                    any_enabled = true;
                }
            }
            Err(e) => {
                tracing::debug!(
                    "Failed to read charging node {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }

    // input_suspend-style nodes:
    // 0 = input is NOT suspended => charging allowed
    for node in SUSPEND_NODES {
        let path = Path::new(node);

        if !path.exists() {
            continue;
        }

        any_node_found = true;

        match fs::read_to_string(path) {
            Ok(content) => {
                any_read_success = true;

                if content.trim() == "0" {
                    any_enabled = true;
                }
            }
            Err(e) => {
                tracing::debug!(
                    "Failed to read suspend node {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }

    if !any_node_found {
        return Err(ChargerError::NoChargingNodeFound);
    }

    // Nodes exist but none could be read.
    if !any_read_success {
        return Err(ChargerError::SysfsWrite {
            path: Path::new("charging_nodes").to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Charging nodes exist but none could be read",
            ),
        });
    }

    Ok(any_enabled)
}
```

### Yang berubah paling penting

Dengan implementasi ini:

```text
Node A = berhasil
Node B = berhasil
Node C = berhasil

=> Ok(ChargingWriteResult {
    attempted: 3,
    succeeded: 3,
    failed: 0
})
```

Partial:

```text
Node A = berhasil
Node B = gagal
Node C = berhasil

=> Ok(ChargingWriteResult {
    attempted: 3,
    succeeded: 2,
    failed: 1
})
```

dan:

```rust
result.partial_failure()
```

akan menghasilkan `true`.

Sedangkan semua gagal:

```text
Node A = gagal
Node B = gagal
Node C = gagal

=> Err(SysfsWrite { ... })
```

Dan tidak ada node:

```text
Tidak ada node yang ditemukan

=> Err(NoChargingNodeFound)
```

### Tapi ada satu perbaikan penting di `HardwareController`

Dengan kontrak baru ini, **kode Anda sebelumnya masih belum sepenuhnya benar** karena ini:

```rust
match control::set_charging(enable) {
    Ok(result) => {
        tracing::info!("Set charging to {}: {:?}", enable, result);
        self.mark_apply_success(target);
    }
    Err(e) => {
        ...
        self.mark_apply_failed();
    }
}
```

masih menganggap **partial failure sebagai success**.

Sebaiknya menjadi:

```rust
match control::set_charging(enable) {
    Ok(result) if result.all_succeeded() => {
        tracing::info!(
            "Set charging to {} successfully: {}/{} nodes",
            enable,
            result.succeeded,
            result.attempted
        );

        self.mark_apply_success(target);
    }

    Ok(result) => {
        tracing::error!(
            "Partial charging control failure: {}/{} succeeded, {} failed",
            result.succeeded,
            result.attempted,
            result.failed
        );

        self.mark_apply_failed();
    }

    Err(e) => {
        tracing::error!(
            "Failed to {} charging: {}",
            if enable { "enable" } else { "disable" },
            e
        );

        self.mark_apply_failed();
    }
}
```

Ini lebih aman untuk Android karena misalnya kernel memiliki:

```text
battery/charging_enabled      -> berhasil
main/charging_enabled         -> gagal
battery/input_suspend         -> berhasil
```

maka kita **tidak boleh mengatakan hardware sudah tersinkron penuh**. Controller harus masuk `Failed` dan melakukan recovery/verification ulang.

Satu catatan lagi: `is_charging_enabled()` Anda sekarang menggunakan logika **ANY node enabled = charging enabled**. Itu cocok untuk kondisi konservatif, tetapi secara semantik bisa false-positive jika satu node stale/virtual sementara node kontrol sebenarnya disabled. Untuk Android vendor-specific, saya justru menyarankan tahap berikutnya membuat **`ChargingState` berbasis konsensus node + prioritas node**, bukan sekadar `any_enabled`.
