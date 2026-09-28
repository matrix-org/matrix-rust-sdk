use jni::{
    EnvUnowned, JavaVM,
    errors::ThrowRuntimeExAndDefault,
    jni_mangle,
    objects::{JClass, JObject},
};
use tracing::{debug, error};

/// Initialize the Android platform support from the JVM.
///
/// This must be called from the JVM side, with the application's `Context`,
/// before calling `init_platform`. It will initialize the `JavaVM` singleton
/// and set up `rustls-platform-verifier`.
///
/// The Kotlin counterpart looks like this:
///
/// ```kotlin
/// package org.matrix.rustsdk
///
/// object Android {
///     @JvmStatic
///     external fun init(context: android.content.Context)
/// }
/// ```
#[jni_mangle("org.matrix.rustsdk.Android", "init")]
pub fn jni_init<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    context: JObject<'caller>,
) {
    unowned_env
        .with_env(|env| rustls_platform_verifier::android::init_with_env(env, context))
        .resolve::<ThrowRuntimeExAndDefault>();
}

/// Checks if the platform support for Android targets was initialized.
///
/// This checks the JVM side has already called
/// `org.matrix.rustsdk.Android.init(context)` (that being the [jni_init]
/// JNI-exposed function), which instantiates a [JavaVM] that can be later
/// accessed through [JavaVm::singleton].
pub(crate) fn check_initialized() {
    if JavaVM::singleton().is_ok() {
        debug!("Android platform support initialized successfully");
    } else {
        error!(
            "The JavaVM hasn't been initialized: `org.matrix.rustsdk.Android.init(context)` \
             must be called before `init_platform`"
        );
    }
}

/// Attach the current thread to a JVM one.
pub(crate) fn android_attach_current_thread_permanently() -> jni::errors::Result<()> {
    JavaVM::singleton()?.attach_current_thread(|_| Ok(()))
}
