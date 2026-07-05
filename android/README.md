# TollGate Android — Native App

Native Android app for the TollGate network. No WebView, no Tauri, no FCM.

## Architecture

```
┌─────────────────────────────────────────┐
│         Kotlin / Jetpack Compose UI       │
│  (MainActivity, WalletScreen, Diagnostics)│
└──────────────────┬──────────────────────┘
                   │ UniFFI FFI boundary
┌──────────────────┴──────────────────────┐
│              Rust native core             │
│  ┌──────────┐ ┌────────┐ ┌────────────┐ │
│  │ nostr-sdk│ │ cashu  │ │tollgate-core│ │
│  │ (rust-   │ │ (NIP-  │ │ (sans-IO   │ │
│  │  nostr)  │ │  60)   │ │  state)    │ │
│  └──────────┘ └────────┘ └────────────┘ │
│              tokio multi-thread           │
└───────────────────────────────────────────┘
```

| Layer | Technology |
|-------|-----------|
| UI | Kotlin + Jetpack Compose (Material 3) |
| FFI | UniFFI (proc-macro mode, no .udl) |
| Core | Rust (`nostr-sdk` 0.40 + workspace `cashu` crate) |
| Runtime | tokio (multi-thread, 2 workers) |
| Crypto | secp256k1 / NIP-44 (native Rust) |
| Notifications | Foreground service + Nostr WebSocket (no FCM/APNS) |

## Build

```bash
# From repo root:
./android/build-android.sh           # debug APK
./android/build-android.sh release   # release APK
```

Output: `android/app/build/outputs/apk/debug/app-debug.apk`

### Prerequisites

- Rust stable (1.85+) with Android targets:
  ```bash
  rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
  cargo install cargo-ndk
  ```
- Android SDK with NDK 27.x
- JDK 17+

### Manual steps

1. **Build Rust .so:**
   ```bash
   cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 build -p tollgate-android
   ```

2. **Copy to jniLibs:**
   ```bash
   for abi in arm64-v8a armeabi-v7a x86_64; do
     mkdir -p android/app/src/main/jniLibs/$abi
     cp target/$abi/debug/libtollgate_android.so android/app/src/main/jniLibs/$abi/
   done
   ```

3. **Generate Kotlin bindings:**
   ```bash
   cargo run -p tollgate-android --bin uniffi-bindgen -- \
     generate --library target/arm64-v8a/debug/libtollgate_android.so \
     --language kotlin --out-dir android/kotlin
   ```

4. **Build APK:**
   ```bash
   cd android && ./gradlew assembleDebug
   ```

## FIPS Notifications (without FCM)

The `NostrListenerService` is a foreground service that maintains persistent
WebSocket connections to the user's Nostr relays. When a relevant event arrives
(payment, DM, wallet update), it fires a local Android notification.

This replaces Firebase Cloud Messaging entirely — the app holds its own relay
connection and surfaces events in real time, even when backgrounded.

## Features

- **Nostr identity**: generate / restore (nsec1…)
- **Cashu NIP-60**: wallet sync via encrypted Nostr events (kind 7375)
- **Token operations**: deposit, verify (NUT-07 check-state)
- **No FCM**: foreground service + WebSocket relay subscription
