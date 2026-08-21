plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "studio.v15.star2"
    compileSdk = 34

    defaultConfig {
        applicationId = "studio.v15.star2"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.2.4"
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Signed with the debug key on purpose: the APK is sideloaded from
            // v15.studio, not shipped through Play, so a real keystore would be
            // one more secret to manage for no gain today.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
}
