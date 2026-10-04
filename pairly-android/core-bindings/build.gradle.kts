import javax.inject.Inject

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

/**
 * Builds ../pairly-core's pairly-ffi with cargo-ndk and generates its Kotlin bindings with
 * UniFFI (scripts/build-rust.sh). ABIs: -Ppairly.abis=arm64-v8a,armeabi-v7a,x86_64
 * (default: arm64-v8a,x86_64).
 */
abstract class BuildRust : DefaultTask() {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val rustSources: ConfigurableFileCollection

    @get:Input
    abstract val abis: Property<String>

    @get:Internal
    abstract val coreDir: DirectoryProperty

    @get:Internal
    abstract val script: RegularFileProperty

    @get:Internal
    abstract val sdkDir: DirectoryProperty

    @get:Internal
    abstract val ndkDir: DirectoryProperty

    @get:OutputDirectory
    abstract val jniLibsDir: DirectoryProperty

    @get:OutputDirectory
    abstract val kotlinDir: DirectoryProperty

    @get:Inject
    abstract val exec: ExecOperations

    @TaskAction
    fun build() {
        exec.exec {
            workingDir = coreDir.get().asFile
            environment("ANDROID_HOME", sdkDir.get().asFile.absolutePath)
            environment("ANDROID_NDK_HOME", ndkDir.get().asFile.absolutePath)
            commandLine(
                script.get().asFile.absolutePath,
                jniLibsDir.get().asFile.absolutePath,
                kotlinDir.get().asFile.absolutePath,
                abis.get(),
            )
        }
    }
}

val pairlyCoreDir = rootProject.layout.projectDirectory.dir("../pairly-core")

androidComponents {
    onVariants { variant ->
        val task = tasks.register<BuildRust>("buildRust${variant.name.replaceFirstChar(Char::uppercase)}") {
            description = "Builds libpairly_ffi.so and its Kotlin bindings for ${variant.name}."
            rustSources.from(
                fileTree(pairlyCoreDir) {
                    include("Cargo.toml", "Cargo.lock", "crates/*/Cargo.toml", "crates/*/uniffi.toml", "crates/*/src/**")
                },
            )
            abis.set(providers.gradleProperty("pairly.abis").orElse("arm64-v8a,x86_64"))
            coreDir.set(pairlyCoreDir)
            script.set(rootProject.layout.projectDirectory.file("scripts/build-rust.sh"))
            sdkDir.set(sdkComponents.sdkDirectory)
            ndkDir.set(sdkComponents.ndkDirectory)
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(task, BuildRust::jniLibsDir)
        variant.sources.kotlin?.addGeneratedSourceDirectory(task, BuildRust::kotlinDir)
    }
}

dependencies {
    // UniFFI-generated Kotlin calls into libpairly_ffi.so through JNA and uses coroutines for async.
    api("${libs.jna.get()}@aar")
    api(libs.kotlinx.coroutines.android)
}
