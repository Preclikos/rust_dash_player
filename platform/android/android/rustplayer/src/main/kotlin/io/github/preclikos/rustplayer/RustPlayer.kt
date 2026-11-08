package io.github.preclikos.rustplayer

import android.content.Context
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.view.Choreographer
import android.view.Display
import android.view.Surface
import org.json.JSONObject

/**
 * Idiomatic, GENERAL-PURPOSE player (the ExoPlayer/Shaka model): give it a
 * manifest URL + a [RustPlayerProvider] (auth / CDN / DRM, all app-side) and it
 * plays. Events arrive on the main thread via [Listener]. No app-specific
 * concepts live in this library.
 */
class RustPlayer(private val context: Context) {

    companion object {
        /** [setPlaybackRate] bounds (the engine clamps to these). */
        const val MIN_PLAYBACK_RATE = 0.5
        const val MAX_PLAYBACK_RATE = 2.0
    }

    /** Player events, delivered on the main thread. All methods are optional. */
    interface Listener {
        fun onPrepared() {}
        fun onTracks(json: String) {}
        fun onPlaying() {}
        fun onPaused() {}
        @Deprecated("override onBuffering(reason) - a spinner should not be shown for every cause")
        fun onBuffering() {}

        /**
         * Playback is waiting for media. [reason] says WHY, which is what
         * decides whether a spinner belongs on screen:
         *
         *  - `"initial"`      first fill after start.
         *  - `"seek"`         the user scrubbed - they expect to wait.
         *  - `"track_switch"` the user picked a track - so do they.
         *  - `"stall"`        ran dry mid-playback; the only unwanted one.
         *
         * An automatic (ABR) quality change never gets here at all: it swaps
         * make-before-break with no buffering event, so a host that shows a
         * spinner for it is reacting to something else.
         *
         * Unknown values may be added later - treat anything unrecognised as
         * a plain stall.
         */
        fun onBuffering(reason: String) {
            @Suppress("DEPRECATION")
            onBuffering()
        }
        fun onPosition(positionMs: Long, durationMs: Long) {}
        fun onVideoSize(width: Int, height: Int) {}
        fun onEnded() {}
        /**
         * Fatal: playback cannot continue. [kind] is stable for branching
         * (`network`, `http_<status>`, `interceptor`, `license_resolver`,
         * `manifest_parse`, `decoder`, `audio_output`, `gpu_unsupported`,
         * `other`); [detail] is for logs, not for the viewer.
         *
         * `gpu_unsupported` arrives right after [start] when the device's GPU
         * cannot run the player (no OpenGL ES 3.0, e.g. Mali-450 on older
         * Amlogic boxes such as the Mi Box 4) AND [start] got no video
         * Surface; with one, such a device plays without a GPU. No player
         * was created ([isStarted] stays false) and retrying cannot help:
         * show an "unsupported device" message or hand the item to another
         * player.
         */
        fun onError(kind: String, detail: String) {}

        /**
         * The selected subtitle track did not load (its file, or a font the
         * manifest marks essential); not fatal, playback goes on without it.
         * [kind] is spelled like [onError]'s (`http_404`, `network`, ...).
         */
        fun onSubtitleTrackFailed(representationId: Int, kind: String, detail: String) {}

        /**
         * Raw 1 Hz stats JSON (debug-HUD food): decoder, frames
         * decoded/dropped, av_drift_ms, video/audio_buffer_ahead_ms,
         * video_segment, stall_events, pipeline_retries, render_gap_max_ms,
         * net_stall_ms, width/height. Fields are additive across versions -
         * parse with opt*().
         */
        fun onStats(json: String) {}

        /**
         * Every event exactly as the bridge emitted it, before it is decoded
         * into the typed callbacks above - including event types this
         * [Listener] has no method for. Delivered on the main thread, in
         * order, ahead of the typed call for the same event.
         *
         * Meant for diagnostics: a host can dump these to a file/logcat and
         * reconstruct a session offline (that is what the :app switch-quality
         * harness does). Not needed for normal playback.
         */
        fun onRawEvent(json: String) {}

        /**
         * The pipeline is primed at [positionMs]: first frame at the target on
         * screen, audio pre-roll queued. Once per pipeline build, also while
         * paused (a paused seek ends here, no [onPlaying] follows) — the
         * moment a [resumeAfter] can start playback exactly on time.
         */
        fun onReady(positionMs: Long) {}
    }

    /** [clockPosition]: the media position audible now. */
    data class ClockPosition(
        val positionMs: Long,
        /** False while paused, buffering or before the first frame. */
        val advancing: Boolean,
        /** The playback rate in effect. */
        val rate: Double,
    )

    var listener: Listener? = null

    /**
     * Report the display's timing to the player (default on): direct mode then
     * releases frames the display's presentation deadline plus one vsync early
     * instead of a fixed 50 ms. Set before [start]; off = the fixed 50 ms.
     */
    var reportDisplayTiming: Boolean = true

    /**
     * Snap direct-mode frame releases onto the display's vsync grid (default
     * on; needs [reportDisplayTiming]). Without it the media clock drifts
     * against the display and, every hour or so, frames sit on a vsync boundary
     * for minutes: the display drops and repeats them (micro-stutter). Set
     * before [start].
     */
    var alignToVsync: Boolean = true

    /**
     * Directory where the subtitle fonts a manifest lists are cached, so the
     * next episode naming the same font reads it from disk (null = no cache,
     * the default). Use a folder of its own, e.g. `File(context.cacheDir,
     * "subtitle-fonts")`. The player only adds files and refreshes their
     * modification time on use; limiting the size is up to the app (delete
     * the oldest files by `lastModified`, or the whole folder, at any time).
     * Takes effect from the next subtitle track selection; set it before
     * [start] so the first track uses it too.
     */
    var fontCacheDir: java.io.File? = null
        set(value) {
            field = value
            if (handle != 0L) NativeBridge.nativeSetFontCacheDir(handle, value?.absolutePath)
        }

    private var vsyncCallback: Choreographer.FrameCallback? = null

    private var handle: Long = 0L
    private val main = Handler(Looper.getMainLooper())
    private var bridge: PlayerBridge? = null
    private var displayListener: DisplayManager.DisplayListener? = null
    private var lastDisplayTiming: Triple<Long, Long, Long>? = null

    val isStarted: Boolean get() = handle != 0L

    /**
     * Build the player on the given surfaces and play [manifestUrl].
     *
     * @param provider auth/CDN/DRM hooks (default: identity requests, no keys).
     * @param startFraction resume at 0..1 of duration, or null to start at 0.
     * @param audioPassthrough true/false to force, null for the library default.
     * @param autoSelectSubtitle default-on (ExoPlayer-like); pass false if the
     *   app drives its own subtitle selection.
     * @param preferredAudioLang BCP-47 (e.g. "cs", "en") applied during default
     *   selection — i.e. BEFORE the first frame. Use this instead of a
     *   post-start [selectAudio]: selecting audio after playback starts triggers
     *   a seek-rebuild (and, on a resume, can stall direct-mode decode); a
     *   start-time preference is picked up with no rebuild. null = codec default.
     * @param preferredSubtitleLang BCP-47 applied during default selection;
     *   honoured even when [autoSelectSubtitle] is false. null = auto policy.
     */
    fun start(
        overlay: Surface,
        video: Surface?,
        width: Int,
        height: Int,
        displayHdrTypes: Int,
        manifestUrl: String,
        provider: RustPlayerProvider = object : RustPlayerProvider {},
        startFraction: Float? = null,
        audioPassthrough: Boolean? = null,
        autoSelectSubtitle: Boolean = true,
        preferredAudioLang: String? = null,
        preferredSubtitleLang: String? = null,
        startPositionMs: Long? = null,
        startPaused: Boolean = false,
    ) {
        if (handle != 0L) return
        val b = PlayerBridge(provider) { json -> main.post { dispatch(json) } }
        bridge = b
        handle = NativeBridge.nativeStart(
            context.applicationContext, b, overlay, video, width, height, displayHdrTypes,
            manifestUrl,
            startFraction ?: -1f,
            when (audioPassthrough) {
                null -> -1
                false -> 0
                true -> 1
            },
            autoSelectSubtitle,
            preferredAudioLang,
            preferredSubtitleLang,
            startPositionMs ?: -1L,
            startPaused,
        )
        // 0 = no player (onError already queued, e.g. gpu_unsupported).
        if (handle == 0L) return
        fontCacheDir?.let { NativeBridge.nativeSetFontCacheDir(handle, it.absolutePath) }
        if (reportDisplayTiming) {
            trackDisplayTiming()
            if (alignToVsync) startVsyncSampling()
        }
    }

    /**
     * Report a vsync timestamp about once a second (the grid's phase; its
     * period comes from the display timing). Choreographer frame times are
     * vsync timestamps on CLOCK_MONOTONIC, the clock release stamps use.
     */
    private fun startVsyncSampling() {
        main.post {
            if (handle == 0L) return@post
            val choreographer = Choreographer.getInstance()
            val cb = object : Choreographer.FrameCallback {
                override fun doFrame(frameTimeNanos: Long) {
                    if (vsyncCallback !== this || handle == 0L) return
                    NativeBridge.nativeOnVsync(handle, frameTimeNanos)
                    main.postDelayed({
                        if (vsyncCallback === this) choreographer.postFrameCallback(this)
                    }, 1000)
                }
            }
            vsyncCallback = cb
            choreographer.postFrameCallback(cb)
        }
    }

    /** Test only: skew release stamps by [ppm] (fast vsync-drift repro). */
    fun setTestPresentSkewPpm(ppm: Long) {
        if (handle != 0L) NativeBridge.nativeSetTestPresentSkewPpm(handle, ppm)
    }

    /** Test only: release stamps [percent] of a vsync before their vsync. */
    fun setTestPresentOffsetPercent(percent: Long) {
        if (handle != 0L) NativeBridge.nativeSetTestPresentOffsetPercent(handle, percent)
    }

    /**
     * Report the display's vsync period, presentation deadline and app vsync
     * offset to the player, now and whenever the display changes (adaptive
     * frame rate switching 60 -> 24 Hz, HDMI hot-plug). Direct mode releases
     * each frame the deadline plus one vsync before its display time; a fixed
     * 50 ms was too late on displays with a long deadline at 24 Hz (frames
     * shown a vsync late or dropped: micro-stutter).
     */
    /**
     * Set the display timing by hand (nanoseconds) instead of reading the
     * default display, e.g. when the video plane is on another display. Stops
     * the automatic tracking; call it again on a refresh-rate change.
     */
    fun setDisplayTiming(vsyncPeriodNs: Long, presentationDeadlineNs: Long, appVsyncOffsetNs: Long = 0L) {
        stopDisplayTiming()
        if (handle != 0L) NativeBridge.nativeSetDisplayTiming(handle, vsyncPeriodNs, presentationDeadlineNs, appVsyncOffsetNs)
    }

    private fun stopDisplayTiming() {
        displayListener?.let {
            (context.getSystemService(Context.DISPLAY_SERVICE) as? DisplayManager)?.unregisterDisplayListener(it)
        }
        displayListener = null
        lastDisplayTiming = null
    }

    private fun trackDisplayTiming() {
        val dm = context.getSystemService(Context.DISPLAY_SERVICE) as? DisplayManager ?: return
        val push = push@{
            if (handle == 0L) return@push
            val d = dm.getDisplay(Display.DEFAULT_DISPLAY) ?: return@push
            val rate = d.refreshRate
            val timing = Triple(
                if (rate > 0f) (1_000_000_000.0 / rate).toLong() else 0L,
                d.presentationDeadlineNanos,
                d.appVsyncOffsetNanos,
            )
            if (timing == lastDisplayTiming) return@push
            lastDisplayTiming = timing
            NativeBridge.nativeSetDisplayTiming(handle, timing.first, timing.second, timing.third)
        }
        val listener = object : DisplayManager.DisplayListener {
            override fun onDisplayAdded(displayId: Int) {}
            override fun onDisplayRemoved(displayId: Int) {}
            override fun onDisplayChanged(displayId: Int) {
                if (displayId == Display.DEFAULT_DISPLAY) push()
            }
        }
        dm.registerDisplayListener(listener, main)
        displayListener = listener
        push()
    }

    fun setSize(width: Int, height: Int) {
        if (handle != 0L) NativeBridge.nativeSetSize(handle, width, height)
    }

    fun play() {
        if (handle != 0L) NativeBridge.nativePlay(handle)
    }

    fun pause() {
        if (handle != 0L) NativeBridge.nativePause(handle)
    }

    fun togglePlayPause() {
        if (handle == 0L) return
        if (NativeBridge.nativeIsPaused(handle)) play() else pause()
    }

    val isPaused: Boolean get() = handle != 0L && NativeBridge.nativeIsPaused(handle)

    fun seekTo(positionMs: Long) {
        if (handle != 0L) NativeBridge.nativeSeekMs(handle, positionMs)
    }

    val positionMs: Long get() = if (handle != 0L) NativeBridge.nativePositionMs(handle) else 0L
    val durationMs: Long get() = if (handle != 0L) NativeBridge.nativeDurationMs(handle) else 0L

    /**
     * Playback rate, 1.0 = normal, clamped to [MIN_PLAYBACK_RATE]..[MAX_PLAYBACK_RATE]
     * with the pitch kept. Persists across seeks and track switches. Built for
     * small continuous corrections (watch-party drift). Returns false when the
     * rate cannot be applied — audio going out as a bitstream (passthrough).
     */
    fun setPlaybackRate(rate: Double): Boolean =
        handle != 0L && NativeBridge.nativeSetPlaybackRate(handle, rate)

    /**
     * The media position audible now (the master clock) — finer than
     * [positionMs], which is the last rendered frame. Null without a player.
     */
    fun clockPosition(): ClockPosition? {
        if (handle == 0L) return null
        val o = try {
            JSONObject(NativeBridge.nativeClockPositionJson(handle))
        } catch (e: Exception) {
            return null
        }
        if (!o.has("position_ms")) return null
        return ClockPosition(o.optLong("position_ms"), o.optBoolean("advancing"), o.optDouble("rate", 1.0))
    }

    /**
     * Resume so the current position becomes audible [delayMs] from now (the
     * output latency taken off) — for starting in step across devices. Meant
     * for a primed pipeline (after [Listener.onReady]); any pause, resume or
     * seek cancels it.
     */
    fun resumeAfter(delayMs: Long) {
        if (handle != 0L) NativeBridge.nativeResumeAfter(handle, delayMs)
    }

    fun setVolume(volume: Float) {
        if (handle != 0L) NativeBridge.nativeSetVolume(handle, volume)
    }

    fun tracksJson(): String = if (handle != 0L) NativeBridge.nativeGetTracksJson(handle) else "{}"

    /**
     * Debug HUD snapshot JSON: pipelines (segments, queue, buffer, last
     * download), decoder and output paths (incl. passthrough), sync, network,
     * ABR and the recent event log. Built on call — poll at 1-2 Hz only while
     * a HUD is visible.
     */
    fun debugJson(): String = if (handle != 0L) NativeBridge.nativeDebugJson(handle) else "{}"

    /**
     * Buffer size and network-outage tolerance. Call right after [start] (the
     * first pipeline starts once the manifest is in); later calls apply from
     * the next seek or track change. 0 keeps a field's default.
     *
     * @param maxSecs media downloaded ahead of the picture (default 30).
     * @param minSecs once full, downloading resumes below this; 0 = fill
     *   continuously (default, like ExoPlayer), lower = bursts (saves radio).
     * @param maxMb memory cap for downloaded media per pipeline (default 96).
     * @param outageSecs how long a lost network is ridden out before an error
     *   (default 180); playback shows buffering and continues when it is back.
     */
    fun setBufferConfig(maxSecs: Int = 0, minSecs: Int = 0, maxMb: Int = 0, outageSecs: Int = 0) {
        if (handle != 0L) NativeBridge.nativeSetBufferConfig(handle, maxSecs, minSecs, maxMb, outageSecs)
    }

    /**
     * The same snapshot as ready-made HUD text (one line per '\n'), ending with
     * the [events] newest event-log lines. Identical on every platform.
     */
    fun debugText(events: Int = 8): String =
        if (handle != 0L) NativeBridge.nativeDebugText(handle, events) else ""

    fun selectVideo(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetVideoTrack(handle, adapt, repr)
    }

    /**
     * Soft (ABR-style) quality switch: the running pipeline swaps the video
     * representation make-before-break at the next segment boundary. Audio and
     * the A/V clock are untouched and no `buffering` is emitted, so the viewer
     * should see nothing but the picture changing quality.
     *
     * This is the seamless path the ABR engine uses. [selectVideo] is
     * deliberately NOT this: a user who picked a quality should get it now,
     * which costs a rebuild and a brief spinner.
     */
    fun selectVideoSoft(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetVideoTrackSoft(handle, adapt, repr)
    }

    fun selectVideoAuto() {
        if (handle != 0L) NativeBridge.nativeSetVideoAuto(handle)
    }

    /**
     * Which representations auto quality (ABR) may pick: `"sdr"` (SDR only,
     * by the manifest's HDR signalling, e.g. HDR turned off in settings),
     * `"8bit"` / `"10bit"` (by codec profile), `"hdr"` (HDR preferred), or
     * `"adaptive"` (default: all). Applies from the next ABR decision.
     */
    fun setAbrVideoProfile(profile: String) {
        if (handle != 0L) NativeBridge.nativeSetAbrVideoProfile(handle, profile)
    }

    /**
     * Fetch ClearKey content keys WRAPPED from [url] instead of through the
     * host's resolveKey (see docs/CLEARKEY_WRAPPED_LICENCE.md): per KID an
     * ephemeral ECDH P-256 exchange, HKDF-SHA256 and AES-256-GCM, so no key
     * crosses the wire in the clear. Authorization headers come from the
     * host's `intercept` for kind "license". Call right after start(); keys
     * are resolved when the init segments are parsed.
     *
     * @param clientSecret this app version's secret (base64url, >= 16 bytes),
     *   the one the licence server holds for it. Inject it at build time
     *   (e.g. a BuildConfig field from CI); never commit it.
     * @param secretId the secret's id (scenario A); null = the server finds
     *   the secret from the request proof (scenario B).
     * @return false when [clientSecret] is malformed.
     */
    fun setWrappedLicence(url: String, clientSecret: String, secretId: String? = null): Boolean =
        handle != 0L && NativeBridge.nativeSetWrappedLicence(handle, url, clientSecret, secretId)

    fun selectAudio(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetAudioTrack(handle, adapt, repr)
    }

    fun selectSubtitle(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetSubtitleTrack(handle, adapt, repr)
    }

    fun clearSubtitles() {
        if (handle != 0L) NativeBridge.nativeClearSubtitles(handle)
    }

    // --- generic knobs (ExoPlayer-style) ---

    /** Re-attach on a surface swap, or detach (null) on background. */
    fun setVideoSurface(surface: Surface?) {
        if (handle != 0L) NativeBridge.nativeSetVideoOutputWindow(handle, surface)
    }

    /**
     * Re-point the overlay surface (the one passed to [start]: the picture on
     * the GLES path, subtitles in direct mode) to a new [Surface], or detach it
     * with null. Keeping the player alive across Home → back needs both
     * surfaces handed over again: call `setOverlaySurface(null)` and
     * `setVideoSurface(null)` from `surfaceDestroyed` (the call returns once the
     * player no longer draws into the old window), then the new surfaces from
     * `surfaceCreated`, followed by [setSize]. Without it the player keeps
     * drawing into the destroyed window and the picture never returns.
     * Since 0.1.40.
     */
    fun setOverlaySurface(surface: Surface?) {
        if (handle != 0L) NativeBridge.nativeSetOverlayWindow(handle, surface)
    }

    fun setSubtitleSafeInsetBottom(px: Int) {
        if (handle != 0L) NativeBridge.nativeSetSubtitleSafeInsetBottom(handle, px)
    }

    fun setAdaptiveFrameRate(enabled: Boolean) {
        if (handle != 0L) NativeBridge.nativeSetAdaptiveFrameRate(handle, enabled)
    }

    /** ARGB ints (Android `Color`), like ExoPlayer `CaptionStyleCompat`. */
    fun setSubtitleStyle(textArgb: Int, outlineArgb: Int, sizeScale: Float) {
        if (handle != 0L) NativeBridge.nativeSetSubtitleStyle(handle, textArgb, outlineArgb, sizeScale)
    }

    /** Verbose logging (default off; gates per-frame vsync/HEALTH spam). */
    fun setVerboseLogging(enabled: Boolean) {
        NativeBridge.nativeSetVerboseLogging(enabled)
    }

    fun release() {
        stopDisplayTiming()
        vsyncCallback = null
        if (handle != 0L) {
            NativeBridge.nativeDestroy(handle)
            handle = 0L
            bridge = null
        }
    }

    private fun dispatch(json: String) {
        val l = listener ?: return
        // Raw first: a diagnostic sink must see events we have no typed
        // callback for, and must see them even if the JSON fails to parse.
        l.onRawEvent(json)
        val o = try {
            JSONObject(json)
        } catch (e: Exception) {
            return
        }
        when (o.optString("type")) {
            "prepared" -> l.onPrepared()
            "tracks_ready" -> l.onTracks(tracksJson())
            "ready" -> l.onReady(o.optLong("position_ms"))
            "playing" -> l.onPlaying()
            "paused" -> l.onPaused()
            "buffering" -> l.onBuffering(o.optString("reason", "stall"))
            "position" -> l.onPosition(o.optLong("position_ms"), o.optLong("duration_ms"))
            "video_size" -> {
                val w = o.optInt("width")
                val h = o.optInt("height")
                if (w > 0 && h > 0) l.onVideoSize(w, h)
            }
            "stats" -> {
                val w = o.optInt("width")
                val h = o.optInt("height")
                if (w > 0 && h > 0) l.onVideoSize(w, h)
                l.onStats(json)
            }
            "end_of_stream" -> l.onEnded()
            "error" -> l.onError(o.optString("kind"), o.optString("detail"))
            "subtitle_track_failed" -> l.onSubtitleTrackFailed(
                o.optInt("representation_id"), o.optString("kind"), o.optString("detail")
            )
        }
    }
}
