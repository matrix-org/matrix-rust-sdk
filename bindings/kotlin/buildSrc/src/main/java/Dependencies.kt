internal object Versions {
    const val androidGradlePlugin = "7.1.2"
    const val kotlin = "1.6.10"
    const val jUnit = "4.12"
    const val nexusPublishGradlePlugin = "1.1.0"
    const val jna = "5.10.0"
}

internal object BuildPlugins {
    const val android = "com.android.tools.build:gradle:${Versions.androidGradlePlugin}"
    const val kotlin = "org.jetbrains.kotlin:kotlin-gradle-plugin:${Versions.kotlin}"
    const val nexusPublish = "io.github.gradle-nexus:publish-plugin:${Versions.nexusPublishGradlePlugin}"
}

/**
 * To define dependencies
 */
internal object Dependencies {
    const val kotlin = "org.jetbrains.kotlin:kotlin-stdlib-jdk7:${Versions.kotlin}"
    const val junit = "junit:junit:${Versions.jUnit}"
    const val jna = "net.java.dev.jna:jna:${Versions.jna}@aar"
}