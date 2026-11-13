plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "dev.henos.android"
    compileSdk = 37

    defaultConfig {
        applicationId = "dev.henos.android"
        minSdk = 26
        targetSdk = 37
        versionCode = 2
        versionName = "0.2.0"
        ndk {
            // Real phones only; emulator-only chip types are left out.
            abiFilters += listOf("arm64-v8a", "armeabi-v7a")
        }
    }

    // Release builds are signed with the project's own key when one is
    // supplied through the environment (as the release workflow does).
    // Without one they fall back to the local debug key, so the build still
    // installs, but such builds cannot update one another across machines.
    val keystorePath = System.getenv("HENOS_KEYSTORE")
    signingConfigs {
        if (keystorePath != null) {
            create("release") {
                storeFile = file(keystorePath)
                storePassword = System.getenv("HENOS_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("HENOS_KEY_ALIAS")
                keyPassword = System.getenv("HENOS_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }

    packaging {
        jniLibs {
            // The bindings library ships loaders for chip types Android phones
            // do not use.
            excludes += listOf("lib/armeabi/**", "lib/mips/**", "lib/mips64/**", "lib/x86/**", "lib/x86_64/**")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.09.00"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.core:core-ktx:1.19.1")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.11.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")
    // QR scanning through Google Play services: no camera permission needed.
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")
    // Required by the generated bindings to the Rust core.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
}
