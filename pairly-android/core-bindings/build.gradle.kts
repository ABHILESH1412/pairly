plugins {
    alias(libs.plugins.android.library)
}

android {
    namespace = "dev.pairly.core"
    compileSdk = 37
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    // UniFFI-generated Kotlin calls into libpairly_ffi.so through JNA and uses coroutines for async.
    api("${libs.jna.get()}@aar")
    api(libs.kotlinx.coroutines.android)
}
