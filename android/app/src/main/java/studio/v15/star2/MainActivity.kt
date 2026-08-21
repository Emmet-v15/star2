package studio.v15.star2

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : AppCompatActivity() {

    private lateinit var roomField: EditText
    private lateinit var callButton: Button
    private lateinit var logView: TextView

    private val poller = Handler(Looper.getMainLooper())
    private val lines = ArrayDeque<String>()

    private val pump = object : Runnable {
        override fun run() {
            Engine.drain().forEach(::say)
            poller.postDelayed(this, 250)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Engine.init(this)
        setContentView(buildUi())
        say("star2 ${Engine.version()}")
        poller.post(pump)
    }

    private fun buildUi(): LinearLayout {
        val pad = (16 * resources.displayMetrics.density).toInt()

        roomField = EditText(this).apply {
            hint = "room token (blank makes a new room)"
            inputType = InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            setSingleLine()
        }

        callButton = Button(this).apply {
            text = "Connect"
            setOnClickListener { toggleCall() }
        }

        logView = TextView(this).apply {
            textSize = 13f
            setTextIsSelectable(true)
        }

        val log = ScrollView(this).apply {
            addView(logView)
            layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f)
        }

        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.TOP
            setPadding(pad, pad, pad, pad)
            addView(roomField, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(callButton, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(log)
        }
    }

    private fun toggleCall() {
        if (Engine.isRunning) {
            Engine.stop()
            callButton.text = "Connect"
            return
        }

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            ActivityCompat.requestPermissions(this, arrayOf(Manifest.permission.RECORD_AUDIO), 1)
            return
        }

        // Blank means "make me a room" - same rule as the desktop prompt, and the
        // token comes back from the same Rust code so both sides agree on format.
        val token = Engine.roomToken(roomField.text.toString())
        roomField.setText(token)

        if (Engine.start(token, android.os.Build.MODEL ?: "android")) {
            callButton.text = "Hang up"
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
            toggleCall()
        } else {
            say("microphone permission denied - cannot call")
        }
    }

    private fun say(line: String) {
        if (lines.size >= 200) lines.removeFirst()
        lines.addLast(line)
        logView.text = lines.joinToString("\n")
    }

    override fun onDestroy() {
        poller.removeCallbacks(pump)
        Engine.stop()
        super.onDestroy()
    }
}
