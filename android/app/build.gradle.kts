plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "network.tollgate.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "network.tollgate.android"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        vectorDrawables { useSupportLibrary = true }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }

    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
    // UniFFI-generated Kotlin bindings live in android/kotlin/ and are added
    // as a source directory so they compile alongside the hand-written Kotlin.
    sourceSets["main"].java.srcDir("../kotlin")
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.androidx.material.icons.extended)
    implementation(libs.androidx.navigation.compose)

    // UniFFI-generated Kotlin bindings (checked into android/kotlin/, added as
    // a source set in app/build.gradle.kts — no fileTree needed).
    implementation(fileTree("kotlin") {})

    // OkHttp: WebSocket client for the Nostr relay listener foreground service.
    // This replaces FCM/APNS — the service holds its own relay connections.
    implementation("com.squareup.okhttp3:okhttp:4.12.0")

    // JNA: required by the UniFFI 0.32 Kotlin bindings (com.sun.jna.Native.register
    // maps the external Rust fns to UniffiLib; com.sun.jna.internal.Cleaner backs
    // the resource cleanup). The @aar variant ships Android-native glue.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
}
