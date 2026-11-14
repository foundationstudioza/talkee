import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:talkee_app/devices.dart';

const _methods = MethodChannel('net.talkee/capture');

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  final calls = <MethodCall>[];
  var peers = <String>[];

  setUp(() {
    calls.clear();
    peers = [
      jsonEncode({
        'id': 'ab' * 16,
        'name': 'compaq',
        'kind': 'server',
        'caps': ['agents'],
        'addr': '192.168.137.2:47810',
      }),
    ];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(_methods, (call) async {
          calls.add(call);
          switch (call.method) {
            case 'meshPeers':
              return peers;
            case 'meshForget':
              peers = [];
              return true;
            case 'meshCall':
              final op = (call.arguments as Map)['op'];
              return op == 'device.info'
                  ? jsonEncode({'name': 'compaq', 'kind': 'server'})
                  : jsonEncode({'uptime': 42});
          }
          return null;
        });
  });

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(_methods, null);
  });

  testWidgets('lists the joined devices, never a key', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: DevicesScreen()));
    await tester.pumpAndSettle();
    expect(find.text('compaq'), findsOneWidget);
    expect(find.textContaining('192.168.137.2:47810'), findsOneWidget);
    expect(find.text('Status'), findsOneWidget);
    expect(find.text('Forget'), findsOneWidget);
  });

  testWidgets('Status asks device.info and system.status and shows both', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: DevicesScreen()));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Status'));
    await tester.pumpAndSettle();
    expect(find.textContaining('kind: server'), findsOneWidget);
    expect(find.textContaining('uptime: 42'), findsOneWidget);
    final ops = calls.where((c) => c.method == 'meshCall').map((c) => (c.arguments as Map)['op']);
    expect(ops, ['device.info', 'system.status']);
  });

  testWidgets('Forget asks first, then removes the device', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: DevicesScreen()));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Forget'));
    await tester.pumpAndSettle();
    expect(calls.where((c) => c.method == 'meshForget'), isEmpty);
    await tester.tap(find.text('Forget').last);
    await tester.pumpAndSettle();
    expect(calls.where((c) => c.method == 'meshForget'), hasLength(1));
    expect(find.text('compaq'), findsNothing);
    expect(find.textContaining('No devices yet'), findsOneWidget);
  });

  test('describeAnswer flattens nested answers', () {
    expect(describeAnswer({'a': 1, 'b': {'c': 2}}), 'a: 1\nb:\n  c: 2');
  });
}
