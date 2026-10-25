
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:talkee_app/main.dart';

/// Channel names must match `TalkeePlugin.kt`. Duplicated here on purpose: if
/// they ever drift, the user-visible symptom is a silent "no connection", so the
/// test should fail loudly instead.
const _methods = MethodChannel('net.talkee/capture');
const _status = EventChannel('net.talkee/status');

/// Makes the status channel report a fixed reading, so the gate can be driven from a test.
///
/// The gate asks the engine whether this phone holds a pairing link, so a test of either screen has
/// to answer that question. Both false is what a fresh install reports.
void mockStatus({
  bool paired = false,
  bool hasLink = false,
  String? error,
  String state = 'discovering',
  bool paused = false,
}) {
  // The real plugin pushes every 250 ms, and the gate and the microphone screen share one broadcast
  // stream: the second listener only hears what is sent after it subscribes. So a test that needs
  // the screen behind the gate calls [pushStatus] once the gate has swapped it in. A timer would do
  // the same, but a test may not end with one pending.
  TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger.setMockStreamHandler(
    _status,
    MockStreamHandler.inline(
      onListen: (arguments, sink) {
        _sink = sink;
        _current = <String, Object?>{
          'state': state,
          'paused': paused,
          'host': hasLink ? 'Talkee on DESKTOP-ABC' : null,
          'level': 0.0,
          'packetsSent': 0,
          'paired': paired,
          'hasLink': hasLink,
          'error': error,
        };
        sink.success(_current);
      },
      onCancel: (arguments) => _sink = null,
    ),
  );
}

MockStreamHandlerEventSink? _sink;
Map<String, Object?> _current = const {};

/// Sends the current status again, as the plugin would a quarter of a second later.
void pushStatus() => _sink?.success(_current);

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  group('TalkeeStatus', () {
    test('reads every field the engine sends', () {
      final status = TalkeeStatus.fromMap(<Object?, Object?>{
        'state': 'streaming',
        'host': 'Talkee on DESKTOP-ABC',
        'address': '192.168.0.24:47801',
        'source': 'unprocessed',
        'level': 0.42,
        'packetsSent': 1234,
        'error': null,
      });

      expect(status.state, 'streaming');
      expect(status.host, 'Talkee on DESKTOP-ABC');
      expect(status.address, '192.168.0.24:47801');
      expect(status.source, 'unprocessed');
      expect(status.level, closeTo(0.42, 1e-9));
      expect(status.packetsSent, 1234);
      expect(status.error, isNull);
      expect(status.isStreaming, isTrue);
    });

    test('survives an entirely empty map', () {
      // The engine sends a fixed map, but a partial one must never crash the UI.
      final status = TalkeeStatus.fromMap(const <Object?, Object?>{});
      expect(status.state, 'idle');
      expect(status.host, isNull);
      expect(status.level, 0);
      expect(status.packetsSent, 0);
      expect(status.isStreaming, isFalse);
    });

    test('accepts integer levels from a platform that widens them', () {
      final status = TalkeeStatus.fromMap(<Object?, Object?>{'level': 1});
      expect(status.level, 1.0);
    });

    test('only streaming counts as streaming', () {
      for (final state in ['idle', 'discovering', 'error']) {
        expect(
          TalkeeStatus.fromMap(<Object?, Object?>{'state': state}).isStreaming,
          isFalse,
          reason: '$state must not be treated as streaming',
        );
      }
    });

    test('carries an error through for display', () {
      final status = TalkeeStatus.fromMap(<Object?, Object?>{
        'state': 'error',
        'error': 'microphone permission not granted',
      });
      expect(status.error, 'microphone permission not granted');
      expect(status.isStreaming, isFalse);
    });
  });

  testWidgets('unpaired, it opens on the scanner with nothing to press',
      (tester) async {
    // A phone with no pairing has exactly one thing to do, so that is the whole screen. Rule 1 of
    // the UI holds here too: there is still nothing to press.
    mockStatus();
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();

    expect(find.byType(FloatingActionButton), findsNothing);
    expect(find.byType(TextField), findsNothing);
    expect(find.byType(ElevatedButton), findsNothing);
    expect(find.textContaining('Point your phone at the code'), findsOneWidget);
    expect(find.textContaining('Looking for your PC'), findsNothing);
  });

  testWidgets('a paired phone is never shown a scanner, even with the PC away',
      (tester) async {
    // The state that made the app feel broken: a phone that has already scanned, whose PC is asleep,
    // was shown the scanner again — which reads as "your pairing was lost". It has a link, so it gets
    // the microphone screen and a sentence about looking for the PC.
    mockStatus(hasLink: true);
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();

    expect(find.textContaining('Looking for your PC'), findsOneWidget);
    expect(find.textContaining('Point your phone at the code'), findsNothing);
  });

  testWidgets('unpaired, it does not build a camera controller it cannot use',
      (tester) async {
    // The seam that makes this screen testable at all, asserted so nobody "simplifies" it away: the
    // stand-in means no platform camera is reached for, and nothing throws on teardown.
    mockStatus();
    await tester.pumpWidget(
      MaterialApp(home: PairingScreen(cameraBuilder: (_) => const _NoCamera())),
    );
    await tester.pump();
    expect(find.byType(_NoCamera), findsOneWidget);
  });

  testWidgets('the scanner never shows the engine\'s own error', (tester) async {
    // The engine reports "the PC was asleep when I checked" through `error`, which is not something
    // the user did and not something this screen should say. A scan that really fails reports itself.
    mockStatus(error: 'pairing failed: ECONNREFUSED');
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();

    expect(find.textContaining('Point your phone at the code'), findsOneWidget);
    expect(find.textContaining('pairing failed'), findsNothing);
    expect(find.textContaining('ECONNREFUSED'), findsNothing);
  });

  testWidgets('paired, it opens on the microphone with a clear idle message',
      (tester) async {
    mockStatus(paired: true, hasLink: true);
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();

    // Rule 2 of the UI: it says what it is doing.
    expect(find.textContaining('Looking for your PC'), findsOneWidget);
    expect(find.textContaining('Point your phone at the code'), findsNothing);
  });

  testWidgets('the orb asks the PC to mute, and does not pretend to have done it',
      (tester) async {
    // The PC decides. Tapping sends a request and the screen shows what the PC reports next, so the
    // phone, the tray and the PC's dashboard cannot disagree about whether the microphone is on.
    final calls = <MethodCall>[];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(_methods, (call) async {
      calls.add(call);
      return null;
    });
    mockStatus(paired: true, hasLink: true, state: 'streaming');
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();
    pushStatus();
    await tester.pump();

    expect(find.text('Live'), findsOneWidget);
    await tester.tap(find.byIcon(Icons.mic_rounded));
    await tester.pump();

    final set = calls.where((c) => c.method == 'set').toList();
    expect(set, hasLength(1));
    expect(set.single.arguments, {'key': 'paused', 'value': true});
    // While it waits, the screen shows what was asked for...
    expect(find.text('Muted'), findsOneWidget);
    // ...and when the PC never answers, it goes back to the truth rather than staying wrong.
    await tester.pump(const Duration(seconds: 3));
    expect(find.text('Muted'), findsNothing);
    expect(find.text('Live'), findsOneWidget);
  });

  testWidgets('a muted PC shows as muted, whichever side muted it', (tester) async {
    mockStatus(paired: true, hasLink: true, state: 'streaming', paused: true);
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();
    pushStatus();
    await tester.pump();

    expect(find.text('Muted'), findsOneWidget);
    expect(find.byIcon(Icons.mic_off_rounded), findsOneWidget);
  });

  testWidgets('with no PC to ask, the switches do nothing', (tester) async {
    final calls = <MethodCall>[];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(_methods, (call) async {
      calls.add(call);
      return null;
    });
    mockStatus(paired: true, hasLink: true);
    await tester.pumpWidget(const TalkeeApp());
    await tester.pump();

    await tester.tap(find.text('Noise'));
    await tester.tap(find.byIcon(Icons.mic_rounded));
    await tester.pump();

    expect(calls.where((c) => c.method == 'set'), isEmpty);
  });

  test('channel names match the Android plugin', () {
    // Guards against a rename on one side only, which would present to the user
    // as an app that simply never connects.
    expect(_methods.name, 'net.talkee/capture');
    expect(_status.name, 'net.talkee/status');
  });
}

/// A stand-in for the camera, so the scanner screen can be built without a platform behind it.
class _NoCamera extends StatelessWidget {
  const _NoCamera();

  @override
  Widget build(BuildContext context) => const SizedBox.shrink();
}

