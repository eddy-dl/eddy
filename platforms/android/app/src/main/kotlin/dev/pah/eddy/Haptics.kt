package dev.pah.eddy

import android.content.Context
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager

// Vibrations for a few events, each its own so they can be told apart
// without looking. Only when Settings → General → haptic feedback is on
// (Rust saves it in the same preferences).
object Haptics {
    // Must match platform::Haptic in Rust.
    const val LINK_CHECKED = 0
    const val DOWNLOADED = 1
    const val FAILED = 2
    const val CLICK = 3

    private const val PREFS = "app_prefs"
    private const val ENABLED_KEY = "enable_haptic_feedback"

    fun play(context: Context, kind: Int) {
        val enabled = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(ENABLED_KEY, "false") == "true"
        if (!enabled) return
        val vibrator = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            (context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager).defaultVibrator
        } else {
            @Suppress("DEPRECATION")
            context.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
        }
        if (!vibrator.hasVibrator()) return
        vibrator.vibrate(effect(kind))
    }

    private fun effect(kind: Int): VibrationEffect {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            when (kind) {
                // A tiny tick: links get checked often.
                LINK_CHECKED -> return VibrationEffect.createPredefined(VibrationEffect.EFFECT_TICK)
                DOWNLOADED -> return VibrationEffect.createPredefined(VibrationEffect.EFFECT_DOUBLE_CLICK)
                CLICK -> return VibrationEffect.createPredefined(VibrationEffect.EFFECT_CLICK)
            }
        }
        return when (kind) {
            LINK_CHECKED -> VibrationEffect.createOneShot(10, 60)
            DOWNLOADED -> VibrationEffect.createWaveform(longArrayOf(0, 30, 80, 30), -1)
            // Three firmer buzzes, unlike anything else.
            FAILED -> VibrationEffect.createWaveform(
                longArrayOf(0, 70, 70, 70, 70, 70),
                intArrayOf(0, 255, 0, 255, 0, 255),
                -1,
            )
            else -> VibrationEffect.createOneShot(25, VibrationEffect.DEFAULT_AMPLITUDE)
        }
    }
}
