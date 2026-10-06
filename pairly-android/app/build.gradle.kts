import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "dev.pairly.android"
    compileSdk = 37
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        applicationId = "io.github.abhilesh1412.pairly"
        minSdk = 26
        targetSdk = 37
        versionCode = 2
        versionName = "0.1.1"
    }

    // Release signing comes from keystore.properties next to this project (never committed):
    //   storeFile=/path/to/pairly-release.jks
    //   keyAlias=pairly
    // The passwords come from PAIRLY_STORE_PASSWORD / PAIRLY_KEY_PASSWORD (scripts/release.sh
    // asks for them), or storePassword= / keyPassword= lines in the file if you prefer.
    // Without it, release builds are signed with the debug key: fine for testing on your own
    // phone, never for publishing (scripts/release.sh refuses).
    val signing = rootProject.file("keystore.properties").takeIf { it.exists() }?.let { file ->
        Properties().apply { file.inputStream().use(::load) }
    }
    signingConfigs {
        if (signing != null) {
            create("release") {
                storeFile = file(signing.getProperty("storeFile"))
                storePassword = signing.getProperty("storePassword") ?: System.getenv("PAIRLY_STORE_PASSWORD")
                keyAlias = signing.getProperty("keyAlias")
                keyPassword = signing.getProperty("keyPassword")
                    ?: System.getenv("PAIRLY_KEY_PASSWORD")
                    ?: storePassword
            }
        }
    }

    buildTypes {
        // Development builds install next to the released app ("Pairly Dev"), since they can't
        // be signed with the release key; the release keeps the plain application id.
        debug {
            applicationIdSuffix = ".dev"
        }
        release {
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }

    packaging {
        jniLibs {
            // JNA ships libjnidispatch.so already stripped; skip AGP's strip attempt.
            keepDebugSymbols += "**/libjnidispatch.so"
        }
    }
}

dependencies {
    implementation(project(":core-bindings"))

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.material.icons)
    implementation(libs.androidx.camera.camera2)
    implementation(libs.androidx.camera.lifecycle)
    implementation(libs.androidx.camera.view)
    implementation(libs.androidx.exifinterface)
    implementation(libs.zxing.core)
    debugImplementation(libs.androidx.compose.ui.tooling)
}
