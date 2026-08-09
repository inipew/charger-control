# Laporan Implementasi Lengkap P3: Engineering Quality (Refactoring & Reliability)

Dokumen ini merekam secara komprehensif seluruh evolusi, keputusan arsitektur, modifikasi tingkat rendah, dan eksekusi kode yang dilakukan selama fase **P3 (Engineering Quality)**. Fase ini mengacu pada perencanaan dari `analisa_25.md` dan pemantapan invarian dari `analisa_26.md`.

Fokus utama dari fase P3 adalah mengubah arsitektur *proof-of-concept* (PoC) yang awalnya sangat bergatung pada eksekusi sistem operasi secara langsung menjadi sistem *production-ready* yang mudah diuji, dapat diprediksi (*deterministic*), dan aman (*fail-safe*).

---

## 1. Abstraksi dan Injeksi Dependensi (Dependency Injection)

### Masalah Sebelumnya
`HardwareController` dan fungsionalitas di dalam modul `battery::control` berinteraksi langsung dengan sistem berkas (menggunakan `std::fs::read_to_string`, `std::fs::write`, dan `std::path::Path::exists()`). Akibatnya, logika bisnis mustahil diuji di dalam *unit test* standar karena membutuhkan sistem berkas sysfs `/sys/class/power_supply` asli dari kernel Android.

### Detail Perubahan
1. **Penciptaan Trait Abstraksi**: Dibuat Trait `HardwareIo`, `PersistenceIo`, dan `Clock` untuk membungkus (*wrapper*) interaksi perangkat keras.
2. **Penggantian Fungsi Standar**: Segala pemanggilan `std::fs::*` di dalam `charger-core` telah dimusnahkan. Khususnya `path.exists()` diganti menjadi pemanggilan `io.exists(path)`.
3. **Pemisahan `HardwareProfile`**: Konfigurasi path node yang di-*hardcode* dipindahkan ke *struct* `HardwareProfile`.

```rust
// crates/charger-core/src/hardware/io.rs
pub trait HardwareIo: Send + Sync {
    fn read(&self, path: &Path) -> Result<String, ChargerError>;
    fn write(&self, path: &Path, value: &str) -> Result<(), ChargerError>;
    
    // Menggantikan pemanggilan `std::path::Path::exists()`
    fn exists(&self, path: &Path) -> bool; 
}
```

---

## 2. Manajemen Kepemilikan Perangkat Cerdas (Ownership & Crash Recovery)

### Masalah Sebelumnya
Jika *daemon* mati paksa (seperti saat *crash* atau di-kill secara paksa oleh Android Low Memory Killer) saat baterai sedang dalam status pengisian terputus (Bypass), *hardware* selamanya akan tertahan dalam mode Bypass sampai pengguna menyalakan ulang (*reboot*) HP mereka.

### Detail Perubahan
1. **Sistem Jejak Kepemilikan (Ownership State)**: Menggunakan modul `ownership.rs`, *daemon* sekarang akan mencatat (persisten) status original `charging_enabled` ke dalam direktori `/data/adb/charger-control/ownership.state` sebelum daemon secara radikal mematikan sirkuit pengisian daya.
2. **Mekanisme *Stale Recovery***: Ketika daemon kembali dinyalakan, proses inisialisasi akan melirik `ownership.state`. Jika *file* itu ada, maka proses sebelumnya mengalami *crash*. Ia akan mengembalikan hardware ke status aslinya, lalu membersihkan *file* kepemilikan tersebut.

```rust
// crates/charger-core/src/persistence/ownership.rs
pub fn recover_stale_ownership(profile: &HardwareProfile, hw_io: &dyn HardwareIo, pers_io: &dyn PersistenceIo) -> Result<RecoveryStatus, ChargerError> {
    let Some(original) = load_persistent_ownership(pers_io) else {
        return Ok(RecoveryStatus::NotNeeded);
    };

    // Eksekusi pemulihan hardware
    match control::set_charging(original, profile, hw_io) {
        Ok(res) if res.all_succeeded() => {
            clear_persistent_ownership(pers_io); // Hapus jika sukses
            Ok(RecoveryStatus::Recovered)
        }
        // Jika gagal karena permissions / EPERM, file dipertahankan agar dicoba di kesempatan berikutnya.
    }
}
```

---

## 3. Observabilitas Tersentralisasi (Controller Events)

### Masalah Sebelumnya
`HardwareController` bercampur aduk antara mengatur state (*domain logic*) dan mengirimkan pesan logging ke layar serta mem-broadcast event ke netlink IPC socket. Hal ini melanggar konsep *Separation of Concerns*.

### Detail Perubahan
`HardwareController::apply_target` dan `HardwareController::verify` dibersihkan dari fungsi logging eksternal dan IPC. Sebagai gantinya, mereka memutar kembalian vektor `Vec<ControllerEvent>`. Lapisan abstraksi terluar (`charger-daemon/src/monitor/mod.rs`) yang bertanggung jawab mengeksekusi efek samping dari tiap event tersebut.

```rust
pub enum ControllerEvent {
    ApplySuccess(HardwareTarget),
    ApplyFailed,
    VerificationSuccess,
    VerificationFailed(u8), // Berisi penghitung percobaan gagal yang sedang berjalan
    ExternalModificationDetected, // Apabila Magisk / Kernel mencoba merusak state secara eksternal
}
```

---

## 4. Penanganan Invarian & Toleransi Kegagalan (Berdasarkan Analisa 26)

Tiga tes invariasi krusial ditambahkan pada `crates/charger-core/src/hardware/controller_test.rs` menggunakan Mocking Objects `MockHardwareIo` dan waktu simulasi `FakeClock`:

1. **`ownership_invariant`**: Memvalidasi siklus penuh *ownership*. Tes membuktikan bahwa status hardware yang tercatat sebagai "1" (mengisi daya) tersimpan ke file, dan berhasil dihapus ketika pelepasan (*unmanaged*) dieksekusi.
2. **`partial_write_invariant`**: Mensimulasikan kesalahan intermiten seperti kernel menolak penulisan pada `/sys/class/power_supply/battery/charging_enabled` (dengan melontarkan *virtual* `ErrorKind::PermissionDenied`). Kode membuktikan *SyncState* controller menolak mengaku sukses (`Synced`) dan merosot ke status `Failed` untuk menjamin percobaan mundur (eksponensial *retry backoff*).
3. **`verification_invariant`**: Melakukan pengujian di mana daemon telah mendisiplinkan baterai (contoh: Bypass Arus), tetapi secara mendadak terdeteksi `current_ma` lebih dari `100mA` saat verifikasi acak berjalan. Controller memicu `SyncState::Unknown` dan mencoba rekonsiliasi ulang lewat event `ExternalModificationDetected`.

---

## 5. Pembersihan Detail dan Peningkatan Kualitas Kode Harian (Micro-refactoring)

Melalui proses analisis mutasi dan peringatan kode (*cargo clippy*), kami menuntaskan detail kecil berikut:

1. **Penyederhanaan `SensorSnapshot`**: Dulu modul sensor menghasilkan struktur berat yang memuat info voltase, suhu, status kernel, dsb. Pada P3, disederhanakan hanya `current_ma: Option<i32>`. Penyesuaian konversi dari MicroAmpere (μA) ke MilliAmpere (mA) kini terjadi jauh sebelum data tersebut sampai ke `HardwareController`.
2. **Penanganan Error Idiomatis (Clippy fixes)**: 
   - Memodernisasi `std::io::Error::new(std::io::ErrorKind::Other, "...")` ke penulisan singkat `std::io::Error::other("...")`.
   - Menghapus konstruksi ambigu `Ok(value.ok_or(err)?)` pada modul pembacaan (`reader.rs`) menjadi `value.ok_or(err)` secara langsung.
3. **Kompilasi Lintas-Platform yang Akurat**:
   - Terjadi kekeliruan `#[cfg(target_os = "linux")]` yang mana ternyata akan diabaikan oleh perakit OS Android murni. Kondisional diubah secara global menjadi `#[cfg(any(target_os = "linux", target_os = "android"))]` untuk mencegah modul terpotong saat dipasang pada gawai Android.
   - Pintu masuk eksekusi `charger-daemon` dan `charger-ctl` memiliki cadangan semu `#[cfg(not(any(target_os = "linux", target_os = "android")))]` untuk memberitahu Visual Studio Code (di Windows) agar *cargo check* dapat tetap sukses tanpa mengaduh tidak bisa menemukan kernel linux `libc` dan `epoll`.

---

## Kesimpulan Arsitektur Akhir P3

Proyek ini telah berkembang dari skrip Bash/Rust *proof-of-concept* menjadi implementasi **Clean Architecture / Hexagonal Architecture** yang andal di level sistem.

*   **Lapisan Dalam (Domain):** `charger-core`, bertindak sebagai *pure decision engine*.
*   **Lapisan Antarmuka (Port):** Trait I/O dan Mock Object untuk Unit Test 100% deterministic.
*   **Lapisan Luar (Adapter):** `charger-daemon` bertindak sebagai penyambung dunia nyata yang *asynchronous* melalui `tokio` (menyentuh UNIX socket).

Kini *daemon* siap dilanjutkan ke iterasi optimasi performa dan alokasi memori (P4) atau peluncuran kandidat rilis (*Release Candidate*).
