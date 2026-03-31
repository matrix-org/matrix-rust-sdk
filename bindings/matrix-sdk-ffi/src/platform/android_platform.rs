use std::{error::Error, mem::MaybeUninit};

use jni::{
    errors::JniError,
    sys::{JavaVM as RawJavaVM, JNI_OK},
};
use tracing::debug;

static ANDROID_JVM: once_cell::sync::OnceCell<jni::JavaVM> = once_cell::sync::OnceCell::new();

/// Initialize the platform support for Android targets.
///
/// This includes setting up `rustls-platform-verifier`.
pub(crate) fn init() {
    debug!("Initializing Android platform support");

    ANDROID_JVM.get_or_init(|| {
        match get_java_vm() {
            Ok(jvm) => {
                // Initialize rustls platform verifier
                let mut env =
                    jvm.attach_current_thread_permanently().expect("Failed to attach thread");
                init_rustls_platform_verifier(&mut env)
                    .expect("Failed to initialize rustls platform verifier");

                debug!("Android platform support initialized successfully");

                jvm
            }
            Err(e) => {
                panic!("Failed to initialize Android platform support: {}", e);
            }
        }
    });
}

fn get_java_vm() -> Result<jni::JavaVM, Box<dyn Error>> {
    debug!("Getting a JVM pointer");
    #[allow(non_snake_case)]
    let JNI_GetCreatedJavaVMs = unsafe {
        jvm_getter::find_jni_get_created_java_vms().expect("Failed to find JNI_GetCreatedJavaVMs")
    };

    let mut vm: MaybeUninit<*mut RawJavaVM> = MaybeUninit::uninit();
    let status = unsafe { JNI_GetCreatedJavaVMs(vm.as_mut_ptr(), 1, &mut 0) };
    if status != JNI_OK {
        panic!("no JavaVM was found by JNI_GetCreatedJavaVMs");
    }

    unsafe { jni::JavaVM::from_raw(vm.assume_init()).map_err(|e| e.into()) }
}

fn init_rustls_platform_verifier(env: &mut jni::JNIEnv<'_>) -> jni::errors::Result<()> {
    // Get the current activity thread
    let activity_thread = env
        .call_static_method(
            "android/app/ActivityThread",
            "currentActivityThread",
            "()Landroid/app/ActivityThread;",
            &[],
        )?
        .l()?;

    // Then get the application context
    let context = env
        .call_method(activity_thread, "getApplication", "()Landroid/app/Application;", &[])?
        .l()?;

    Ok(rustls_platform_verifier::android::init_hosted(env, context)?)
}

/// Attach the current thread to a JVM one.
pub(crate) fn android_attach_current_thread_permanently(
) -> jni::errors::Result<jni::JNIEnv<'static>> {
    ANDROID_JVM
        .get()
        .ok_or_else(|| jni::errors::Error::JniCall(JniError::Unknown))?
        .attach_current_thread_permanently()
}
