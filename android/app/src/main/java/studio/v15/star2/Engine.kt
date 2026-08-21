package studio.v15.star2

import android.content.Context

/**
 * Thin binding over the same Rust engine the desktop client runs. Everything
 * below is implemented in crates/star2-android.
 */
object Engine {
    init {
        System.loadLibrary("star2_android")
    }

    private var handle: Long = 0

    val isRunning: Boolean get() = handle != 0L

    /**
     * cpal reaches the audio devices through the Android context, so this has
     * to run before any call starts. Safe to call more than once.
     */
    fun init(context: Context) = nativeInit(context.applicationContext)

    fun start(room: String, name: String): Boolean {
        if (handle != 0L) return true
        handle = nativeStart(room, name)
        return handle != 0L
    }

    fun stop() {
        if (handle == 0L) return
        nativeStop(handle)
        handle = 0
    }

    /** Drains whatever the engine has said since the last call. */
    fun drain(): List<String> =
        nativePoll().split("\n").filter { it.isNotBlank() }

    fun roomToken(label: String): String = nativeNewRoomToken(label)

    fun version(): String = nativeVersion()

    private external fun nativeInit(context: Context)
    private external fun nativeStart(room: String, name: String): Long
    private external fun nativeStop(handle: Long)
    private external fun nativePoll(): String
    private external fun nativeNewRoomToken(label: String): String
    private external fun nativeVersion(): String
}
