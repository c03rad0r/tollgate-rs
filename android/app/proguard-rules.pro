# Keep UniFFI-generated classes (reflection used by JNI)
-keep class uniffi.** { *; }
-keep class network.tollgate.android.** { *; }

# OkHttp (uses reflection for platform detection)
-dontwarn okhttp3.**
-dontwarn okio.**
