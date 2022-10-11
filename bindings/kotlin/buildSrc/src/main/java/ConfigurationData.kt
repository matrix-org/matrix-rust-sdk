object ConfigurationData {
    const val compileSdk = 31
    const val targetSdk = 31
    const val minSdk = 21
    const val majorVersion = 0
    const val minorVersion = 2
    const val patchVersion = 0
    const val versionName = "$majorVersion.$minorVersion.$patchVersion"
    const val snapshotVersionName = "$majorVersion.$minorVersion.${patchVersion + 1}-SNAPSHOT"
    const val publishGroupId = "org.matrix.rustcomponents"
}