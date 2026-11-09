import java.util.Properties

plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// Firebase client config is not in the repo. The google-services plugin fails the build when
// google-services.json is missing, so apply it only when the file is there. Without it the
// app still builds and runs, but push registration fails at runtime (Firebase cannot
// initialise) and the Settings screen reports that. See app/NOTIFICATIONS.md.
if (file("google-services.json").exists()) {
    apply(plugin = "com.google.gms.google-services")
} else {
    logger.warn("google-services.json not found in android/app: building without Firebase config, push will not work")
}

// Release signing comes from android/key.properties (gitignored, see key.properties.example)
// or from BREADTRACKER_* environment variables, so a CI runner can inject it. Nothing here
// is ever committed.
val keyProps = Properties().apply {
    val f = rootProject.file("key.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}

fun signingValue(prop: String, env: String): String? =
    (keyProps.getProperty(prop) ?: System.getenv(env))?.takeIf { it.isNotBlank() }

val releaseStoreFile = signingValue("storeFile", "BREADTRACKER_KEYSTORE")
val releaseStorePassword = signingValue("storePassword", "BREADTRACKER_KEYSTORE_PASSWORD")
val releaseKeyAlias = signingValue("keyAlias", "BREADTRACKER_KEY_ALIAS")
val releaseKeyPassword = signingValue("keyPassword", "BREADTRACKER_KEY_PASSWORD")
val hasReleaseSigning = listOf(releaseStoreFile, releaseStorePassword, releaseKeyAlias, releaseKeyPassword).all { it != null }

android {
    namespace = "dev.breadway.tracker"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "dev.breadway.tracker"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        minSdk = flutter.minSdkVersion
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = file(releaseStoreFile!!)
                storePassword = releaseStorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        release {
            // Never fall back to the debug key: a release APK signed with it cannot be
            // updated by a properly signed one. Without signing config, release tasks fail.
            if (hasReleaseSigning) signingConfig = signingConfigs.getByName("release")
        }
    }
}

gradle.taskGraph.whenReady {
    val releaseTask = allTasks.any { t ->
        t.project == project && (t.name.startsWith("assemble") || t.name.startsWith("bundle")) && t.name.endsWith("Release")
    }
    if (releaseTask && !hasReleaseSigning) {
        throw GradleException(
            "Release signing is not configured. Create android/key.properties (see key.properties.example) " +
                "or set BREADTRACKER_KEYSTORE, BREADTRACKER_KEYSTORE_PASSWORD, BREADTRACKER_KEY_ALIAS and BREADTRACKER_KEY_PASSWORD."
        )
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

dependencies {
    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:2.1.4")
}

flutter {
    source = "../.."
}
