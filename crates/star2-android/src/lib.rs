use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;

use star2_engine::{start_call, CallConfig, CallHandle, Event};

static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CONTEXT_READY: AtomicBool = AtomicBool::new(false);

const MAX_QUEUED: usize = 200;

fn push(line: String) {
    let mut q = EVENTS.lock().unwrap();
    if q.len() >= MAX_QUEUED {
        q.remove(0);
    }
    q.push(line);
}

fn take_string(env: &mut JNIEnv, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(|v| v.into())
}

#[no_mangle]
pub extern "system" fn Java_studio_v15_star2_Engine_nativeInit(
    env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    if CONTEXT_READY.swap(true, Ordering::SeqCst) {
        return;
    }

    let (Ok(vm), Ok(ctx)) = (env.get_java_vm(), env.new_global_ref(context)) else {
        CONTEXT_READY.store(false, Ordering::SeqCst);
        push("could not reach the Android audio context".into());
        return;
    };

    unsafe {
        ndk_context::initialize_android_context(
            vm.get_java_vm_pointer().cast(),
            ctx.as_raw().cast(),
        );
    }
    std::mem::forget(ctx);
}

#[no_mangle]
pub extern "system" fn Java_studio_v15_star2_Engine_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    room: JString,
    name: JString,
) -> jlong {
    let (Some(room), Some(name)) = (take_string(&mut env, &room), take_string(&mut env, &name))
    else {
        push("could not read room or name".into());
        return 0;
    };

    let mut cfg = CallConfig::default();
    if !room.trim().is_empty() {
        cfg.room_token = room.trim().to_string();
    }
    if !name.trim().is_empty() {
        cfg.name = name.trim().to_string();
    }
    cfg.stereo = false;
    cfg.bitrate = 64_000;

    EVENTS.lock().unwrap().clear();
    push(format!("joining {}", cfg.room_token));

    match start_call(cfg, |e| match e {
        Event::Status(s) => push(s),
        Event::Direct(addr) => push(format!("connected - direct path to {addr}")),
        Event::Ended(why) => push(format!("call ended: {why}")),
        Event::Stats { .. } => {}
    }) {
        Ok(handle) => Box::into_raw(Box::new(handle)) as jlong,
        Err(e) => {
            push(format!("could not start: {e:#}"));
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_studio_v15_star2_Engine_nativeStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    let call = unsafe { Box::from_raw(handle as *mut CallHandle) };
    call.stop();
    push("hung up".into());
}

#[no_mangle]
pub extern "system" fn Java_studio_v15_star2_Engine_nativePoll(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let lines: Vec<String> = EVENTS.lock().unwrap().drain(..).collect();
    match env.new_string(lines.join("\n")) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_studio_v15_star2_Engine_nativeNewRoomToken(
    mut env: JNIEnv,
    _class: JClass,
    label: JString,
) -> jstring {
    let label = take_string(&mut env, &label).unwrap_or_default();
    let token = if star2_proto::is_room_token(label.trim()) {
        label.trim().to_string()
    } else {
        star2_proto::new_room_token(label.trim())
    };
    match env.new_string(token) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_studio_v15_star2_Engine_nativeVersion(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    match env.new_string(env!("CARGO_PKG_VERSION")) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}
