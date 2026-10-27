plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

android {
    namespace = "net.talkee.talkee_app"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        // TODO: Specify your own unique Application ID (https://developer.android.com/studio/build/application-id.html).
        applicationId = "net.talkee.talkee_app"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        // 26 (Android 8.0) rather than Flutter's default: we need
        // NotificationChannel, Context.startForegroundService and
        // AudioRecord.read(float[]) without version guards, and 8.0 covers
        // effectively every device in use.
        minSdk = 26
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    // The release key. It comes from the environment, which is where the release workflow puts it,
    // and every build signed with it can update every other: Android refuses an update signed with a
    // different key, so this key must never change once a friend has installed the app.
    //
    // With no key in the environment (a developer machine) the build falls back to the debug key, so
    // `flutter run --release` still works — but that build cannot be updated from a release.
    val releaseKeystore = System.getenv("TALKEE_KEYSTORE")
    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = System.getenv("TALKEE_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("TALKEE_KEY_ALIAS")
                keyPassword = System.getenv("TALKEE_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        debug {
            // Installs beside the release app instead of over it. A debug build is signed with a
            // different key, and Android will not update an app with a build signed by another key, so
            // sharing an id would force an uninstall — and a fresh pairing — every time a developer
            // tested on a phone that also runs the released app.
            applicationIdSuffix = ".dev"
            versionNameSuffix = "-dev"
        }
        release {
            signingConfig = if (releaseKeystore != null) {
                signingConfigs.getByName("release")
            } else {
                signingConfigs.getByName("debug")
            }
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

dependencies {
    // JVM unit tests for the parts of the engine that have no Android dependencies —
    // `Crypto`, `PairLink` and `Control` in particular, checked against the vectors and the live
    // host the Rust side provides without needing a device.
    testImplementation("junit:junit:4.13.2")
    // The framework's org.json is Android's, so a JVM test needs its own copy of the same API.
    testImplementation("org.json:json:20240303")
}
