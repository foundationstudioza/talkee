package net.talkee.talkee_app

import android.content.Context
import android.content.Intent
import android.media.AudioFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import android.util.Log
import java.io.OutputStream
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * Live words on the phone while a dictation records: for looks only.
 *
 * The text that gets typed always comes from the PC's cloud pipeline after the button is released.
 * This shows roughly what is being said, while it is said, using Android's own speech recognizer,
 * on the device where the phone has an on-device one. It costs nothing and sends nothing anywhere
 * new when on-device recognition is available.
 *
 * Talkee already holds the microphone to stream to the PC, and a recognizer opening the microphone
 * again would hear nothing on many phones. So it is fed Talkee's own captured audio through a pipe
 * ([RecognizerIntent.EXTRA_AUDIO_SOURCE], Android 13). Older phones get no preview.
 *
 * The capture thread must never wait on the recognizer: frames go through a small bounded queue to
 * a writer thread, and are dropped when it is full. A preview that skips is fine; a stream to the PC
 * that stalls is not.
 */
class DictationPreview(
    private val context: Context,
    private val onText: (String) -> Unit,
) {
    private val main = Handler(Looper.getMainLooper())
    private var recognizer: SpeechRecognizer? = null
    private var writeEnd: ParcelFileDescriptor? = null
    private var readEnd: ParcelFileDescriptor? = null
    private val queue = ArrayBlockingQueue<ShortArray>(64)
    @Volatile private var feeding = false
    private var writer: Thread? = null

    /** Starts recognizing. Call from any thread. */
    fun start() {
        if (!supported(context)) return
        val pipe = try {
            ParcelFileDescriptor.createPipe()
        } catch (e: Exception) {
            Log.w(TAG, "no pipe for the preview: $e")
            return
        }
        readEnd = pipe[0]
        writeEnd = pipe[1]
        queue.clear()
        feeding = true
        val out: OutputStream = ParcelFileDescriptor.AutoCloseOutputStream(pipe[1])
        writer = Thread({
            try {
                while (feeding || queue.isNotEmpty()) {
                    val frame = queue.poll(100, TimeUnit.MILLISECONDS) ?: continue
                    val bytes = ByteArray(frame.size * 2)
                    for (i in frame.indices) {
                        bytes[2 * i] = (frame[i].toInt() and 0xFF).toByte()
                        bytes[2 * i + 1] = (frame[i].toInt() shr 8).toByte()
                    }
                    out.write(bytes, 0, frame.size * 2)
                }
            } catch (_: Exception) {
                // The recognizer closed its end: nothing left to feed.
            } finally {
                runCatching { out.close() }
            }
        }, "talkee-preview-feed").apply { isDaemon = true; start() }

        main.post {
            try {
                val created = if (Build.VERSION.SDK_INT >= 33 &&
                    SpeechRecognizer.isOnDeviceRecognitionAvailable(context)
                ) {
                    SpeechRecognizer.createOnDeviceSpeechRecognizer(context)
                } else {
                    SpeechRecognizer.createSpeechRecognizer(context)
                }
                created.setRecognitionListener(listener)
                val intent = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH)
                    .putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
                    .putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
                    .putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, true)
                    .putExtra(RecognizerIntent.EXTRA_AUDIO_SOURCE, readEnd)
                    .putExtra(RecognizerIntent.EXTRA_AUDIO_SOURCE_CHANNEL_COUNT, 1)
                    .putExtra(RecognizerIntent.EXTRA_AUDIO_SOURCE_ENCODING, AudioFormat.ENCODING_PCM_16BIT)
                    .putExtra(RecognizerIntent.EXTRA_AUDIO_SOURCE_SAMPLING_RATE, 16_000)
                    // Do not end at the first pause: the person is still holding the button.
                    .putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS, 10_000L)
                    .putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_POSSIBLY_COMPLETE_SILENCE_LENGTH_MILLIS, 10_000L)
                created.startListening(intent)
                recognizer = created
            } catch (e: Exception) {
                Log.w(TAG, "no live preview: $e")
                stop()
            }
        }
    }

    /**
     * One 48 kHz mono capture frame. Called on the capture thread; never blocks. Averaged down to
     * 16 kHz, which is what the recognizer is told it gets.
     */
    fun feed(frame: FloatArray, count: Int) {
        if (!feeding) return
        val out = ShortArray(count / 3)
        for (i in out.indices) {
            val mean = (frame[3 * i] + frame[3 * i + 1] + frame[3 * i + 2]) / 3f
            out[i] = (mean.coerceIn(-1f, 1f) * 32767f).toInt().toShort()
        }
        queue.offer(out) // dropped when full, by design
    }

    /** Stops feeding and lets the recognizer finish what it has. */
    fun stop() {
        feeding = false
        val r = recognizer
        recognizer = null
        main.post {
            runCatching { r?.stopListening() }
            // Give it a moment to deliver its last words, then let it go.
            main.postDelayed({ runCatching { r?.destroy() } }, 1_500)
        }
        runCatching { readEnd?.close() }
        readEnd = null
        writeEnd = null
    }

    private val listener = object : RecognitionListener {
        override fun onPartialResults(partialResults: Bundle?) = deliver(partialResults)
        override fun onResults(results: Bundle?) = deliver(results)
        override fun onError(error: Int) {
            Log.i(TAG, "preview recognizer ended: $error")
        }
        override fun onReadyForSpeech(params: Bundle?) {}
        override fun onBeginningOfSpeech() {}
        override fun onRmsChanged(rmsdB: Float) {}
        override fun onBufferReceived(buffer: ByteArray?) {}
        override fun onEndOfSpeech() {}
        override fun onEvent(eventType: Int, params: Bundle?) {}

        private fun deliver(bundle: Bundle?) {
            val text = bundle
                ?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                ?.firstOrNull()
                ?.trim()
            if (!text.isNullOrEmpty()) onText(text)
        }
    }

    companion object {
        private const val TAG = "talkee-preview"

        /** Whether this phone can show a preview at all. */
        fun supported(context: Context): Boolean =
            Build.VERSION.SDK_INT >= 33 && SpeechRecognizer.isRecognitionAvailable(context)
    }
}
