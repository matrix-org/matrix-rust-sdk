use std::{error::Error, mem::MaybeUninit};

use jni::{errors::JniError, jni_sig, jni_str, sys::JNI_OK};
use jni::sys::JavaVM;
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
                jvm.attach_current_thread(|env| {
                    init_rustls_platform_verifier(env)
                }).expect("Failed to initialize rustls platform verifier");

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

    let mut vm: MaybeUninit<*mut JavaVM> = MaybeUninit::uninit();
    let status = unsafe { JNI_GetCreatedJavaVMs(vm.as_mut_ptr(), 1, &mut 0) };
    if status != JNI_OK {
        panic!("no JavaVM was found by JNI_GetCreatedJavaVMs");
    }

    Ok(unsafe { jni::JavaVM::from_raw(vm.assume_init()) })
}

fn init_rustls_platform_verifier(env: &mut jni::Env<'_>) -> jni::errors::Result<()> {
    // Get the current activity thread
    let class = env
        .find_class(jni_str!("android/app/ActivityThread"))?;
    let activity_thread = env
        .call_static_method(
            class,
            jni_str!("currentActivityThread"),
            jni_sig!("()Landroid/app/ActivityThread;"),
            &[],
        )?
        .l()?;

    // Then get the application context
    let context = env
        .call_method(activity_thread, jni_str!("getApplication"), jni_sig!("()Landroid/app/Application;"), &[])?
        .l()?;

    Ok(rustls_platform_verifier::android::init_with_env(env, context)?)
}

/// Attach the current thread to a JVM one.
pub(crate) fn android_attach_current_thread_permanently()
-> jni::errors::Result<()> {
    ANDROID_JVM
        .get()
        .ok_or_else(|| jni::errors::Error::JniCall(JniError::Unknown))?
        .attach_current_thread(|_| Ok(()))
}
