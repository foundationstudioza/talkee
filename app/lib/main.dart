import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

/// Method channel exposed by `TalkeePlugin.kt`. Three verbs only: start, stop,
/// status. Anything that would need a fourth belongs in the engine, not the UI.
const MethodChannel _methods = MethodChannel('net.talkee/capture');

/// Pushed from Kotlin four times a second — fast enough for a level meter,
/// slow enough to be free.
const EventChannel _statusEvents = EventChannel('net.talkee/status');

void main() {
  runApp(const TalkeeApp());
}

/// A snapshot of the phone-side pipeline.
@immutable
class TalkeeStatus {
  const TalkeeStatus({
    required this.state,
    this.host,
    this.address,
    this.source,
    this.level = 0,
    this.packetsSent = 0,
    this.paired = false,
    this.hasLink = false,
    this.paused = false,
    this.noiseSuppression = true,
    this.echoCancel = true,
    this.update,
    this.backgroundAllowed = true,
    this.error,
    this.dictateSupported = false,
    this.dictating = false,
    this.dictatePhase = 'idle',
    this.dictateText,
    this.dictateError,
    this.previewText,
    this.dictateEnabled = true,
    this.dictateCleanup = true,
    this.vocabulary = const [],
    this.usedSeconds = -1,
    this.limitSeconds = -1,
  });

  factory TalkeeStatus.fromMap(Map<Object?, Object?> map) => TalkeeStatus(
    state: (map['state'] as String?) ?? 'idle',
    host: map['host'] as String?,
    address: map['address'] as String?,
    source: map['source'] as String?,
    level: (map['level'] as num?)?.toDouble() ?? 0,
    packetsSent: (map['packetsSent'] as num?)?.toInt() ?? 0,
    paired: (map['paired'] as bool?) ?? false,
    hasLink: (map['hasLink'] as bool?) ?? false,
    paused: (map['paused'] as bool?) ?? false,
    noiseSuppression: (map['noiseSuppression'] as bool?) ?? true,
    echoCancel: (map['echoCancel'] as bool?) ?? true,
    update: map['update'] as String?,
    backgroundAllowed: (map['backgroundAllowed'] as bool?) ?? true,
    error: map['error'] as String?,
    dictateSupported: (map['dictateSupported'] as bool?) ?? false,
    dictating: (map['dictating'] as bool?) ?? false,
    dictatePhase: (map['dictatePhase'] as String?) ?? 'idle',
    dictateText: map['dictateText'] as String?,
    dictateError: map['dictateError'] as String?,
    previewText: map['previewText'] as String?,
    dictateEnabled: (map['dictateEnabled'] as bool?) ?? true,
    dictateCleanup: (map['dictateCleanup'] as bool?) ?? true,
    vocabulary:
        (map['vocabulary'] as List<Object?>?)?.whereType<String>().toList() ??
        const [],
    usedSeconds: (map['usedSeconds'] as num?)?.toInt() ?? -1,
    limitSeconds: (map['limitSeconds'] as num?)?.toInt() ?? -1,
  );

  /// One of `idle`, `discovering`, `streaming`, `error`.
  final String state;
  final String? host;
  final String? address;
  final String? source;

  /// Peak level of the most recent frame, 0..1.
  final double level;
  final int packetsSent;

  /// True when frames are sealed, which is to say a session is live with the PC.
  ///
  /// Distinct from [host]: discovery can fill that in too, and a phone that found its PC by mDNS is
  /// streaming in the clear.
  final bool paired;

  /// True when this phone has a pairing link, whether or not the PC is answering right now.
  ///
  /// This is what decides which screen the app is. A paired phone whose PC is asleep still has a
  /// link, and showing it a scanner again would suggest the pairing was lost.
  final bool hasLink;

  /// False when the system may stop the app once the screen is off, which on many phones it will
  /// within minutes. The screen offers the one tap that lets it keep running.
  final bool backgroundAllowed;

  /// The PC's switches, as it last reported them. The PC decides; this only shows.
  final bool paused;
  final bool noiseSuppression;
  final bool echoCancel;

  /// The version of a downloaded update waiting to be installed, if there is one.
  final String? update;
  final String? error;

  /// The PC can dictate: it has said so on this connection.
  final bool dictateSupported;

  /// The hold-to-talk button is held.
  final bool dictating;

  /// The PC's dictation phase: `idle`, `recording`, `transcribing`, `done`, or `failed`.
  final String dictatePhase;

  /// The text the PC typed, after `done`.
  final String? dictateText;

  /// Why the last dictation failed, after `failed`.
  final String? dictateError;

  /// What the phone's own recognizer heard so far: a preview, never what gets typed.
  final String? previewText;

  /// The PC's dictation settings. The PC owns them; the phone asks for changes.
  final bool dictateEnabled;
  final bool dictateCleanup;
  final List<String> vocabulary;

  /// Seconds of dictation used today and the daily allowance, or -1 when not known yet.
  final int usedSeconds;
  final int limitSeconds;

  bool get isStreaming => state == 'streaming';
}

/// Status colours, defined once so the whole app speaks with one voice.
class _Palette {
  static const background = Color(0xFF0B0D10);
  static const searching = Color(0xFFFBBF24);
  static const streaming = Color(0xFF4ADE80);
  static const failed = Color(0xFFF87171);
}

class TalkeeApp extends StatelessWidget {
  const TalkeeApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Talkee',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        useMaterial3: true,
        brightness: Brightness.dark,
        scaffoldBackgroundColor: _Palette.background,
        colorScheme: ColorScheme.fromSeed(
          seedColor: _Palette.streaming,
          brightness: Brightness.dark,
        ),
      ),
      home: const _Gate(),
    );
  }
}

/// The microphone screen.
///
/// # Design rules this file obeys
///
/// 1. **Almost nothing to press.** Streaming starts on launch and restarts itself. There are three
///    controls and each is a picture, not a sentence: the orb mutes, and two small switches turn the
///    PC's noise suppression and echo cancellation on and off.
/// 2. **One sentence at a time.** The screen says the name of the PC, or that it is looking for one.
///    Counters, addresses and capture-source names are not for a person holding a phone.
/// 3. **The PC decides.** A switch asks the PC and shows what the PC says back. Nothing here
///    pretends to change state it does not own, which is what keeps the phone, the tray and the
///    PC's dashboard from ever disagreeing.
class TalkeeHome extends StatefulWidget {
  const TalkeeHome({super.key});

  @override
  State<TalkeeHome> createState() => _TalkeeHomeState();
}

class _TalkeeHomeState extends State<TalkeeHome> with WidgetsBindingObserver {
  TalkeeStatus _status = const TalkeeStatus(state: 'idle');
  StreamSubscription<dynamic>? _subscription;
  Timer? _retry;
  Timer? _updateTimer;

  /// A switch the user just pressed, shown as pressed until the PC answers.
  ///
  /// Round trips on a LAN are tens of milliseconds, so this is usually invisible. It exists for the
  /// case where the answer never comes, so the switch snaps back rather than lying.
  final Map<String, bool> _pending = {};
  final Map<String, Timer> _pendingTimers = {};

  /// The finger is on the dictation button.
  bool _holding = false;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _subscription = _statusEvents.receiveBroadcastStream().listen(
      _onStatus,
      onError: (_) {
        // A dropped status stream must not take the app down.
      },
    );
    _start();
    // Looks for a newer build now and every few hours while the app is open. Quiet: an update that
    // is found and downloaded shows up as one line at the top of the screen, and nothing else.
    _invoke('checkUpdate');
    _updateTimer = Timer.periodic(
      const Duration(hours: 6),
      (_) => _invoke('checkUpdate'),
    );
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _subscription?.cancel();
    _retry?.cancel();
    _updateTimer?.cancel();
    for (final timer in _pendingTimers.values) {
      timer.cancel();
    }
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // Returning to the app re-asserts intent. If the system killed capture while
    // we were away, this brings it back without the user asking.
    if (state == AppLifecycleState.resumed && !_status.isStreaming) {
      _start();
    }
  }

  void _onStatus(dynamic raw) {
    if (!mounted || raw is! Map<Object?, Object?>) return;
    final status = TalkeeStatus.fromMap(raw);
    setState(() => _status = status);
    // A tap is settled when the PC reports the value that was asked for, and not before. Status
    // arrives four times a second, so clearing on *any* status wiped the tap before the PC had
    // answered: the switch showed the new state, snapped back to the old one when the next stale
    // status landed, then forward again — and a tap during that flicker undid the first. If the PC
    // never agrees, the timer set in [_ask] gives up and the screen goes back to the truth.
    final reported = <String, bool>{
      'paused': status.paused,
      'noise_suppression': status.noiseSuppression,
      'echo_cancel': status.echoCancel,
    };
    for (final key in _pending.keys.toList()) {
      if (reported[key] == _pending[key]) {
        _pending.remove(key);
        _pendingTimers.remove(key)?.cancel();
      }
    }
    _scheduleRetry(status);
  }

  /// Retries after a failure, but *never* resets an already-armed timer.
  ///
  /// Status arrives four times a second. Cancelling and recreating the timer on
  /// every push would reset it every 250 ms, so it would never fire — a silent
  /// bug that looks exactly like "retry is broken".
  void _scheduleRetry(TalkeeStatus status) {
    if (_retry != null) return;
    if (status.state != 'error') return;
    _retry = Timer(const Duration(seconds: 3), () {
      _retry = null;
      _start();
    });
  }

  /// Invokes a channel method, swallowing the two errors that mean "there is no
  /// plugin here" rather than "something went wrong". Real failures arrive
  /// through the status stream instead.
  Future<void> _invoke(String method, [Object? arguments]) async {
    try {
      await _methods.invokeMethod<void>(method, arguments);
    } on PlatformException {
      // Reported through the status stream.
    } on MissingPluginException {
      // Only reachable in tests and on platforms without the plugin.
    }
  }

  Future<void> _start() => _invoke('start');

  /// The dictation button went down (`true`) or came up (`false`).
  void _dictate(bool down) {
    if (down == _holding) return;
    setState(() => _holding = down);
    if (down) {
      HapticFeedback.mediumImpact();
      _invoke('dictateStart');
    } else {
      HapticFeedback.selectionClick();
      _invoke('dictateStop');
    }
  }

  /// The words to show where the PC's name usually is, or null for the name.
  ///
  /// While recording, what the phone's recognizer hears (grey: a preview). Once the PC has typed
  /// it, the real text.
  String? get _words {
    if (!_status.isStreaming) return null;
    switch (_status.dictatePhase) {
      case 'recording':
        return _status.previewText ?? 'Listening\u2026';
      case 'transcribing':
        return _status.previewText ?? 'Transcribing\u2026';
      case 'done':
        final text = _status.dictateText;
        return (text == null || text.isEmpty) ? null : text;
      default:
        return null;
    }
  }

  bool get _wordsAreFinal => _status.dictatePhase == 'done';

  /// Asks the PC to change a switch. Does nothing while there is no PC to ask.
  void _ask(String key, bool value) {
    if (!_status.isStreaming) return;
    HapticFeedback.selectionClick();
    setState(() => _pending[key] = value);
    _pendingTimers[key]?.cancel();
    _pendingTimers[key] = Timer(const Duration(seconds: 2), () {
      if (mounted) setState(() => _pending.remove(key));
    });
    _invoke('set', {'key': key, 'value': value});
  }

  bool get _paused => _pending['paused'] ?? _status.paused;
  bool get _noise => _pending['noise_suppression'] ?? _status.noiseSuppression;
  bool get _echo => _pending['echo_cancel'] ?? _status.echoCancel;

  String get _headline {
    if (!_status.isStreaming) return 'Looking for your PC…';
    return _status.host ?? 'Connected';
  }

  String get _detail {
    if (!_status.isStreaming) {
      if (_status.state == 'error' && _status.error != null) {
        return '${_status.error}\nOpen Talkee on your computer to connect.\nTap the circle to try again.';
      }
      return 'Open Talkee on your computer to connect.\nTap the circle to try again.';
    }
    switch (_status.dictatePhase) {
      case 'recording':
        return 'Listening\u2026 let go when you are done';
      case 'transcribing':
        return 'Transcribing\u2026';
      case 'done':
        if (_words != null) return 'Typed on ${_status.host ?? 'your PC'}';
      case 'failed':
        if (_status.dictateError != null) return _status.dictateError!;
    }
    return _paused ? 'Muted' : 'Live';
  }

  @override
  Widget build(BuildContext context) {
    final connected = _status.isStreaming;
    final accent = !connected
        ? _Palette.searching
        : _paused
        ? _Palette.failed
        : _Palette.streaming;

    return Scaffold(
      body: SafeArea(
        child: Stack(
          children: [
            if (_status.isStreaming && _status.dictateSupported)
              Align(
                alignment: Alignment.topLeft,
                child: GestureDetector(
                  onTap: () => Navigator.of(context).push(
                    MaterialPageRoute<void>(
                      builder: (_) => DictationSettingsScreen(initial: _status),
                    ),
                  ),
                  behavior: HitTestBehavior.opaque,
                  child: const Padding(
                    padding: EdgeInsets.all(16),
                    child: Icon(
                      Icons.tune_rounded,
                      color: Colors.white30,
                      size: 26,
                    ),
                  ),
                ),
              ),
            // Scan the code again: for a phone whose pairing no longer works — a new PC, a reset key —
            // where retrying can never succeed. Small and quiet, because most days it is never needed.
            Align(
              alignment: Alignment.topRight,
              child: GestureDetector(
                onTap: () => Navigator.of(context).push(
                  MaterialPageRoute<void>(
                    builder: (_) => const PairingScreen(),
                  ),
                ),
                behavior: HitTestBehavior.opaque,
                child: const Padding(
                  padding: EdgeInsets.all(16),
                  child: Icon(
                    Icons.qr_code_scanner_rounded,
                    color: Colors.white30,
                    size: 26,
                  ),
                ),
              ),
            ),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 32),
              child: Column(
                children: [
                  if (_status.update != null) ...[
                    const SizedBox(height: 14),
                    GestureDetector(
                      onTap: () => _invoke('installUpdate'),
                      behavior: HitTestBehavior.opaque,
                      child: Container(
                        padding: const EdgeInsets.symmetric(
                          horizontal: 16,
                          vertical: 10,
                        ),
                        decoration: BoxDecoration(
                          borderRadius: BorderRadius.circular(20),
                          color: _Palette.streaming.withValues(alpha: 0.12),
                          border: Border.all(
                            color: _Palette.streaming.withValues(alpha: 0.5),
                          ),
                        ),
                        child: Text(
                          'Update ${_status.update} ready · Install',
                          style: const TextStyle(
                            fontSize: 13,
                            color: _Palette.streaming,
                          ),
                        ),
                      ),
                    ),
                  ],
                  if (connected && !_status.backgroundAllowed) ...[
                    const SizedBox(height: 14),
                    GestureDetector(
                      onTap: () => _invoke('allowBackground'),
                      behavior: HitTestBehavior.opaque,
                      child: Container(
                        padding: const EdgeInsets.symmetric(
                          horizontal: 16,
                          vertical: 10,
                        ),
                        decoration: BoxDecoration(
                          borderRadius: BorderRadius.circular(20),
                          color: _Palette.searching.withValues(alpha: 0.12),
                          border: Border.all(
                            color: _Palette.searching.withValues(alpha: 0.5),
                          ),
                        ),
                        child: const Text(
                          'Keep working with the screen off · Allow',
                          style: TextStyle(
                            fontSize: 13,
                            color: _Palette.searching,
                          ),
                        ),
                      ),
                    ),
                  ],
                  const Spacer(flex: 3),
                  GestureDetector(
                    // Connected, the orb mutes. Not connected, it is the retry button: a person looking at
                    // "Looking for your PC…" after their phone died wants to say "try now", and the circle
                    // is the one thing on the screen that looks pressable.
                    onTap: connected
                        ? () => _ask('paused', !_paused)
                        : () => _invoke('reconnect'),
                    behavior: HitTestBehavior.opaque,
                    child: _Pulse(
                      level: connected && !_paused ? _status.level : 0,
                      accent: accent,
                      icon: _paused ? Icons.mic_off_rounded : Icons.mic_rounded,
                    ),
                  ),
                  const SizedBox(height: 40),
                  if (_words == null)
                    Text(
                      _headline,
                      textAlign: TextAlign.center,
                      style: const TextStyle(
                        fontSize: 28,
                        fontWeight: FontWeight.w300,
                        letterSpacing: -0.5,
                      ),
                    )
                  else
                    // The words being dictated, where the PC's name usually is. Scrolls, newest at the
                    // bottom, so a long dictation keeps its latest words in view.
                    ConstrainedBox(
                      constraints: const BoxConstraints(maxHeight: 180),
                      child: SingleChildScrollView(
                        reverse: true,
                        child: SelectableText(
                          _words!,
                          textAlign: TextAlign.center,
                          style: TextStyle(
                            fontSize: 20,
                            height: 1.35,
                            fontWeight: FontWeight.w400,
                            color: _wordsAreFinal ? Colors.white : Colors.white54,
                          ),
                        ),
                      ),
                    ),
                  const SizedBox(height: 10),
                  Text(
                    _detail,
                    textAlign: TextAlign.center,
                    style: TextStyle(
                      fontSize: 14,
                      height: 1.5,
                      color: connected
                          ? accent.withValues(alpha: 0.9)
                          : Colors.white38,
                    ),
                  ),
                  const Spacer(flex: 4),
                  if (connected && _status.dictateSupported) ...[
                    _HoldToTalk(holding: _holding, onChange: _dictate),
                    const SizedBox(height: 28),
                  ],
                  Row(
                    mainAxisAlignment: MainAxisAlignment.center,
                    children: [
                      _Switch(
                        icon: Icons.graphic_eq_rounded,
                        label: 'Noise',
                        on: _noise,
                        enabled: connected,
                        onTap: () => _ask('noise_suppression', !_noise),
                      ),
                      const SizedBox(width: 28),
                      _Switch(
                        icon: Icons.speaker_phone_rounded,
                        label: 'Echo',
                        on: _echo,
                        enabled: connected,
                        onTap: () => _ask('echo_cancel', !_echo),
                      ),
                    ],
                  ),
                  const SizedBox(height: 36),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Dictation settings, kept on the PC: on/off, cleanup, today's minutes, and the vocabulary.
///
/// The PC owns them, as it owns the switches: a change here is a request, and the screen shows what
/// the PC reports back, so the phone and the PC window never disagree.
class DictationSettingsScreen extends StatefulWidget {
  const DictationSettingsScreen({super.key, required this.initial});

  final TalkeeStatus initial;

  @override
  State<DictationSettingsScreen> createState() => _DictationSettingsScreenState();
}

class _DictationSettingsScreenState extends State<DictationSettingsScreen> {
  late TalkeeStatus _status = widget.initial;
  StreamSubscription<dynamic>? _watch;
  final TextEditingController _word = TextEditingController();
  String? _note;

  @override
  void initState() {
    super.initState();
    _watch = _statusEvents.receiveBroadcastStream().listen((event) {
      if (!mounted || event is! Map<Object?, Object?>) return;
      setState(() => _status = TalkeeStatus.fromMap(event));
    });
  }

  @override
  void dispose() {
    _watch?.cancel();
    _word.dispose();
    super.dispose();
  }

  Future<void> _configure(Map<String, Object?> change) async {
    try {
      final sent = await _methods.invokeMethod<bool>('dictateConfigure', change);
      if (sent == false && mounted) {
        setState(() => _note = 'Could not reach your PC, or the list is too long.');
      } else if (mounted) {
        setState(() => _note = null);
      }
    } on PlatformException {
      // Reported through the status stream.
    } on MissingPluginException {
      // Tests.
    }
  }

  void _addWord() {
    final word = _word.text.trim();
    if (word.isEmpty) return;
    final exists = _status.vocabulary.any((w) => w.toLowerCase() == word.toLowerCase());
    _word.clear();
    if (!exists) _configure({'vocabulary': [..._status.vocabulary, word]});
  }

  @override
  Widget build(BuildContext context) {
    final used = _status.usedSeconds;
    final limit = _status.limitSeconds;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Dictation'),
        backgroundColor: Colors.transparent,
        elevation: 0,
      ),
      body: SafeArea(
        child: ListView(
          padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 8),
          children: [
            const Text(
              'These settings live on your PC, so the phone and the Talkee window always agree.',
              style: TextStyle(color: Colors.white54, fontSize: 13, height: 1.4),
            ),
            if (limit > 0) ...[
              const SizedBox(height: 12),
              Text(
                '${(used / 60).ceil()} of ${limit ~/ 60} minutes used today',
                style: const TextStyle(color: Colors.white70, fontSize: 14),
              ),
            ],
            const SizedBox(height: 12),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Dictation'),
              subtitle: const Text('Ctrl+Win on the PC and the button here'),
              value: _status.dictateEnabled,
              onChanged: (v) => _configure({'enabled': v}),
            ),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Clean up the text'),
              subtitle: const Text('Punctuation, lists, corrections, no filler words'),
              value: _status.dictateCleanup,
              onChanged: (v) => _configure({'cleanup': v}),
            ),
            const SizedBox(height: 20),
            const Text('Vocabulary', style: TextStyle(fontSize: 17, fontWeight: FontWeight.w500)),
            const SizedBox(height: 4),
            const Text(
              'Names and words Talkee should spell right.',
              style: TextStyle(color: Colors.white54, fontSize: 13),
            ),
            const SizedBox(height: 10),
            Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: _word,
                    textCapitalization: TextCapitalization.words,
                    decoration: const InputDecoration(hintText: 'Add a word, like Compaq'),
                    onSubmitted: (_) => _addWord(),
                  ),
                ),
                IconButton(
                  icon: const Icon(Icons.add_rounded),
                  tooltip: 'Add',
                  onPressed: _addWord,
                ),
              ],
            ),
            if (_note != null) ...[
              const SizedBox(height: 8),
              Text(_note!, style: const TextStyle(color: Color(0xFFF87171), fontSize: 13)),
            ],
            const SizedBox(height: 12),
            Wrap(
              spacing: 8,
              runSpacing: 8,
              children: [
                for (final word in _status.vocabulary)
                  InputChip(
                    label: Text(word),
                    onDeleted: () => _configure({
                      'vocabulary': _status.vocabulary.where((w) => w != word).toList(),
                    }),
                  ),
              ],
            ),
            if (_status.vocabulary.isEmpty)
              const Text('No words yet.', style: TextStyle(color: Colors.white38, fontSize: 13)),
          ],
        ),
      ),
    );
  }
}

/// Hold to dictate: the PC records while the finger is down and types the text when it lifts.
///
/// A [Listener] rather than a gesture recognizer, because "down" has to start recording at once:
/// a long-press recognizer would wait half a second and lose the first words.
class _HoldToTalk extends StatelessWidget {
  const _HoldToTalk({required this.holding, required this.onChange});

  final bool holding;
  final void Function(bool down) onChange;

  @override
  Widget build(BuildContext context) {
    const red = Color(0xFFE5484D);
    return Listener(
      onPointerDown: (_) => onChange(true),
      onPointerUp: (_) => onChange(false),
      onPointerCancel: (_) => onChange(false),
      behavior: HitTestBehavior.opaque,
      child: AnimatedContainer(
        duration: const Duration(milliseconds: 120),
        width: double.infinity,
        height: 64,
        alignment: Alignment.center,
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(32),
          color: holding ? red.withValues(alpha: 0.22) : Colors.white.withValues(alpha: 0.06),
          border: Border.all(
            color: holding ? red : Colors.white24,
            width: holding ? 2 : 1,
          ),
        ),
        child: Row(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Icon(
              Icons.keyboard_voice_rounded,
              color: holding ? red : Colors.white70,
            ),
            const SizedBox(width: 10),
            Text(
              holding ? 'Listening\u2026 let go to type it' : 'Hold to dictate',
              style: TextStyle(
                fontSize: 16,
                color: holding ? Colors.white : Colors.white70,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// The breathing indicator.
///
/// Doubles as a level meter: its radius and glow follow the peak of the most recent frame, and the
/// microphone inside it says whether it is on. That is the whole of the feedback the app gives, and
/// it answers the two questions a user actually has — "is it on?" and "can it hear me?".
class _Pulse extends StatelessWidget {
  const _Pulse({required this.level, required this.accent, required this.icon});

  final double level;
  final Color accent;
  final IconData icon;

  @override
  Widget build(BuildContext context) {
    final intensity = level.clamp(0.0, 1.0);
    return SizedBox(
      width: 220,
      height: 220,
      child: Center(
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 120),
          curve: Curves.easeOut,
          width: 116 + intensity * 72,
          height: 116 + intensity * 72,
          decoration: BoxDecoration(
            shape: BoxShape.circle,
            color: accent.withValues(alpha: 0.12 + intensity * 0.45),
            border: Border.all(
              color: accent.withValues(alpha: 0.55),
              width: 1.5,
            ),
            boxShadow: [
              BoxShadow(
                color: accent.withValues(alpha: 0.30 * intensity),
                blurRadius: 70 * intensity,
                spreadRadius: 6 * intensity,
              ),
            ],
          ),
          child: Icon(icon, size: 40, color: accent.withValues(alpha: 0.95)),
        ),
      ),
    );
  }
}

/// A round icon switch with a one-word caption.
///
/// A picture rather than a row of text: the screen is meant to be read at a glance from across a
/// desk, and a lit circle says "on" faster than a sentence does.
class _Switch extends StatelessWidget {
  const _Switch({
    required this.icon,
    required this.label,
    required this.on,
    required this.enabled,
    required this.onTap,
  });

  final IconData icon;
  final String label;
  final bool on;
  final bool enabled;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final lit = on && enabled;
    final colour = lit ? _Palette.streaming : Colors.white30;
    return GestureDetector(
      onTap: enabled ? onTap : null,
      behavior: HitTestBehavior.opaque,
      child: Opacity(
        opacity: enabled ? 1 : 0.4,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            AnimatedContainer(
              duration: const Duration(milliseconds: 180),
              width: 56,
              height: 56,
              decoration: BoxDecoration(
                shape: BoxShape.circle,
                color: lit ? colour.withValues(alpha: 0.14) : Colors.white10,
                border: Border.all(
                  color: lit ? colour.withValues(alpha: 0.6) : Colors.white12,
                ),
              ),
              child: Icon(icon, color: colour, size: 24),
            ),
            const SizedBox(height: 8),
            Text(
              label,
              style: TextStyle(
                fontSize: 12,
                color: lit ? Colors.white70 : Colors.white30,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// Decides which of the two things the app is: the scanner, or the microphone.
///
/// A phone that has not been paired has exactly one thing to do — scan the code on the PC — so that
/// is the whole screen, in the same voice as the rest of the app: one sentence, nothing to press.
///
/// The answer comes from the engine and nowhere else. That matters for the case that looks like it
/// needs special handling and does not: when the PC rotates its key, this phone's stored link stops
/// working, the engine reports itself unpaired, and the scanner comes back by itself.
class _Gate extends StatefulWidget {
  const _Gate();

  @override
  State<_Gate> createState() => _GateState();
}

class _GateState extends State<_Gate> {
  StreamSubscription<dynamic>? _watch;

  /// Whether the phone holds a pairing link — which is the whole question this widget answers.
  bool _hasLink = false;

  @override
  void initState() {
    super.initState();
    unawaited(_startStreaming());
    // The same channel `TalkeeHome` listens to. `receiveBroadcastStream` shares one underlying
    // subscription per channel, so this costs no extra polling on the platform side.
    _watch = _statusEvents.receiveBroadcastStream().listen((event) {
      if (event is! Map) return;
      final hasLink = TalkeeStatus.fromMap(event).hasLink;
      // Rebuild when the answer changes, not at the status stream's four times a second.
      if (hasLink != _hasLink && mounted) setState(() => _hasLink = hasLink);
    });
  }

  /// Asks the engine to start, and swallows a refusal.
  ///
  /// Not fatal and not worth a dialog: the engine reports what is wrong through the status stream,
  /// and the scanner is still the right screen to be looking at.
  Future<void> _startStreaming() async {
    try {
      await _methods.invokeMethod<void>('start');
    } catch (_) {
      // Reported by the engine, not by the UI.
    }
  }

  @override
  void dispose() {
    _watch?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    // The scanner is for a phone that has never been paired, and nothing else. Once there is a link
    // the microphone screen is the truth, even while it is still looking for the PC — a scanner shown
    // to somebody who has already scanned reads as "your pairing was lost".
    if (_hasLink) return const TalkeeHome();
    // The engine's error is deliberately *not* shown here. It is almost always "the PC was asleep
    // when the app checked", which is not something the user did, and putting it on the scanner made
    // the app accuse them of a failure before they had scanned anything. A scan that really failed
    // reports itself, from the screen that ran it.
    return const PairingScreen();
  }
}

/// The first-run screen: point the phone at the code on the PC.
class PairingScreen extends StatefulWidget {
  const PairingScreen({super.key, this.cameraBuilder});

  /// Builds the viewfinder instead of the real camera.
  ///
  /// A widget test has no camera and no platform behind it, so the alternative is either a mock of
  /// the plugin's internals or no test of this screen at all. A seam is cheaper than both.
  final Widget Function(BuildContext context)? cameraBuilder;

  @override
  State<PairingScreen> createState() => _PairingScreenState();
}

class _PairingScreenState extends State<PairingScreen> {
  /// Created on first use.
  ///
  /// Lazy on purpose: a caller that supplies its own [PairingScreen.cameraBuilder] never creates a
  /// controller, so nothing reaches for a platform that is not there.
  MobileScannerController? _controller;

  MobileScannerController get _camera => _controller ??=
      MobileScannerController(detectionSpeed: DetectionSpeed.noDuplicates);

  /// Set while a scan is being handed over, so a burst of frames is not sent twice.
  bool _handling = false;

  /// What went wrong, in a sentence, or null while everything is fine.
  String? _problem;

  @override
  void dispose() {
    _controller?.dispose();
    super.dispose();
  }

  Future<void> _onDetect(BarcodeCapture capture) async {
    if (_handling) return;
    // Ignore every code that is not ours: the camera sees the whole room, and half of it is not a
    // pairing code. The engine would refuse it anyway, but saying so on every frame would be noise.
    String? link;
    for (final barcode in capture.barcodes) {
      final value = barcode.rawValue;
      if (value != null && value.startsWith('talkee://')) {
        link = value;
        break;
      }
    }
    if (link == null) return;

    setState(() => _handling = true);
    try {
      await _methods.invokeMethod<void>('pair', {'link': link});
      // Opened from the microphone screen, the scanner is a page on top of it: a good scan puts the
      // person back where they were. As the first-run screen there is nothing to go back to, and the
      // gate swaps the screen by itself.
      if (mounted && Navigator.of(context).canPop()) {
        Navigator.of(context).pop();
      }
      // No navigation here. The engine reports `paired` on the status stream, the gate swaps the
      // screen, and there is one source of truth for "are we paired" rather than two that can
      // disagree.
    } on PlatformException catch (e) {
      setState(() {
        _handling = false;
        _problem = e.message ?? 'that was not a Talkee pairing code';
      });
    } catch (e) {
      setState(() {
        _handling = false;
        _problem = 'could not pair: $e';
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: _Palette.background,
      body: Stack(
        fit: StackFit.expand,
        children: [
          widget.cameraBuilder?.call(context) ??
              MobileScanner(
                controller: _camera,
                onDetect: _onDetect,
                // A camera that will not start is a sentence, not a crash: this is the screen a
                // person sees when the permission was refused.
                errorBuilder: (context, error) => const Align(
                  alignment: Alignment(0, -0.35),
                  child: Padding(
                    padding: EdgeInsets.symmetric(horizontal: 48),
                    child: Text(
                      'The camera is off for Talkee.\nAllow it in your phone\'s Settings to scan.',
                      textAlign: TextAlign.center,
                      style: TextStyle(fontSize: 14, height: 1.5, color: Colors.white54),
                    ),
                  ),
                ),
              ),
          if (Navigator.of(context).canPop())
            SafeArea(
              child: Align(
                alignment: Alignment.topLeft,
                child: GestureDetector(
                  onTap: () => Navigator.of(context).pop(),
                  behavior: HitTestBehavior.opaque,
                  child: const Padding(
                    padding: EdgeInsets.all(16),
                    child: Icon(
                      Icons.close_rounded,
                      color: Colors.white70,
                      size: 28,
                    ),
                  ),
                ),
              ),
            ),
          // A frame to aim at. Purely visual — the camera reads the whole image — but a bare camera
          // feed gives a person nothing to line the code up with.
          IgnorePointer(
            child: Center(
              child: Container(
                width: 250,
                height: 250,
                decoration: BoxDecoration(
                  borderRadius: BorderRadius.circular(32),
                  border: Border.all(
                    color: (_problem == null ? Colors.white : _Palette.failed)
                        .withValues(alpha: 0.7),
                    width: 2,
                  ),
                ),
              ),
            ),
          ),
          _Caption(
            // A scan that was just rejected outranks the engine's last complaint: it is the newer
            // of the two and the one the user just caused.
            problem: _problem,
            handling: _handling,
          ),
        ],
      ),
    );
  }
}

/// The one sentence at the bottom of the scanner.
class _Caption extends StatelessWidget {
  const _Caption({required this.problem, required this.handling});

  final String? problem;
  final bool handling;

  @override
  Widget build(BuildContext context) {
    final text =
        problem ??
        (handling
            ? 'Pairing\u2026'
            : 'Point your phone at the code on the screen of your PC');
    final colour = problem == null ? Colors.white70 : _Palette.failed;
    return Align(
      alignment: Alignment.bottomCenter,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(32, 0, 32, 56),
        child: Text(
          text,
          textAlign: TextAlign.center,
          style: TextStyle(fontSize: 15, height: 1.5, color: colour),
        ),
      ),
    );
  }
}
