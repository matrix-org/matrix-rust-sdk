use std::error::Error;

use jni::{
    errors::JniError,
    sys::{jint, jsize, JavaVM},
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

type JniGetCreatedJavaVms =
    unsafe extern "system" fn(_: *mut *mut JavaVM, _: jsize, _: *mut jsize) -> jint;
const JNI_GET_JAVA_VMS_NAME: &[u8] = b"JNI_GetCreatedJavaVMs";

fn get_java_vm() -> Result<jni::JavaVM, Box<dyn Error>> {
    // Use libloading to avoid having to create and expose a JNI function and call
    // it from the Android side.
    let library = libloading::os::unix::Library::this();
    let get_created_java_vms: JniGetCreatedJavaVms =
        unsafe { *library.get(JNI_GET_JAVA_VMS_NAME)? };

    let mut java_vms: [*mut JavaVM; 1] = [std::ptr::null_mut() as *mut JavaVM];
    let mut vm_count: i32 = 0;

    let ok = unsafe { get_created_java_vms(java_vms.as_mut_ptr(), 1, &mut vm_count) };
    if ok != jni::sys::JNI_OK {
        return Err("Failed to get JavaVM".into());
    }
    if vm_count != 1 {
        return Err(format!("Invalid JavaVM count: {vm_count}").into());
    }

    let jvm = unsafe { jni::JavaVM::from_raw(java_vms[0]) }?;
    Ok(jvm)
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
