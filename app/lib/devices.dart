import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

/// Same channel as the rest of the app (`TalkeePlugin.kt`). Keys never come through it: the engine
/// describes a device by id, name, kind and capabilities only.
const MethodChannel _mesh = MethodChannel('net.talkee/capture');

/// A device this phone has joined, as the engine describes it.
@immutable
class MeshDevice {
  const MeshDevice({
    required this.id,
    required this.name,
    required this.kind,
    required this.caps,
    required this.addr,
  });

  final String id;
  final String name;
  final String kind;
  final List<String> caps;
  final String addr;

  static MeshDevice? parse(Object? raw) {
    try {
      final map = raw is String ? jsonDecode(raw) : raw;
      if (map is! Map) return null;
      final id = map['id'];
      if (id is! String || id.isEmpty) return null;
      return MeshDevice(
        id: id,
        name: (map['name'] as String?) ?? id,
        kind: (map['kind'] as String?) ?? 'pc',
        caps: [for (final c in (map['caps'] as List? ?? const [])) c.toString()],
        addr: (map['addr'] as String?) ?? '',
      );
    } catch (_) {
      return null;
    }
  }

  IconData get icon => switch (kind) {
    'server' => Icons.dns_rounded,
    'phone' => Icons.smartphone_rounded,
    _ => Icons.computer_rounded,
  };
}

/// Turns a mesh answer into text a person can read: `key: value` lines, nested ones indented.
String describeAnswer(Object? value, [int depth = 0]) {
  final pad = '  ' * depth;
  if (value is Map) {
    if (value.isEmpty) return '$pad(nothing)';
    return value.entries
        .map((e) {
          final v = e.value;
          if (v is Map || v is List) {
            return '$pad${e.key}:\n${describeAnswer(v, depth + 1)}';
          }
          return '$pad${e.key}: $v';
        })
        .join('\n');
  }
  if (value is List) {
    if (value.isEmpty) return '$pad(none)';
    return value
        .map((v) => v is Map || v is List ? describeAnswer(v, depth + 1) : '$pad- $v')
        .join('\n');
  }
  return '$pad$value';
}

/// The devices this phone has joined: forget one, or ask it how it is.
class DevicesScreen extends StatefulWidget {
  const DevicesScreen({super.key});

  @override
  State<DevicesScreen> createState() => _DevicesScreenState();
}

class _DevicesScreenState extends State<DevicesScreen> {
  List<MeshDevice>? _devices;
  String? _busy;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    List<MeshDevice> found = const [];
    try {
      final raw = await _mesh.invokeMethod<List<Object?>>('meshPeers') ?? const [];
      found = [for (final r in raw) ?MeshDevice.parse(r)];
    } on PlatformException {
      // Shown as an empty list.
    } on MissingPluginException {
      // Tests.
    }
    if (mounted) setState(() => _devices = found);
  }

  Future<void> _forget(MeshDevice d) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Forget ${d.name}?'),
        content: const Text(
          'This phone deletes its key for that device. To add it again, make a new link on it.',
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Keep')),
          TextButton(onPressed: () => Navigator.pop(context, true), child: const Text('Forget')),
        ],
      ),
    );
    if (ok != true) return;
    try {
      await _mesh.invokeMethod<bool>('meshForget', {'id': d.id});
    } on PlatformException {
      // The list below shows whatever is still there.
    }
    await _load();
  }

  Future<String> _ask(MeshDevice d, String op) async {
    try {
      final text = await _mesh.invokeMethod<String>('meshCall', {'id': d.id, 'op': op});
      return describeAnswer(jsonDecode(text ?? 'null'));
    } on PlatformException catch (e) {
      return 'Could not ask: ${e.message ?? 'no answer'}';
    }
  }

  Future<void> _status(MeshDevice d) async {
    setState(() => _busy = d.id);
    // device.info is answered by every node; system.status only by talkeed, so its refusal is not
    // an error worth showing on its own.
    final info = await _ask(d, 'device.info');
    final status = await _ask(d, 'system.status');
    if (!mounted) return;
    setState(() => _busy = null);
    await showDialog<void>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(d.name),
        content: SingleChildScrollView(
          child: Text('$info\n\n$status', style: const TextStyle(fontSize: 13, height: 1.4)),
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context), child: const Text('Close')),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final devices = _devices;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Devices'),
        backgroundColor: Colors.transparent,
        elevation: 0,
      ),
      body: SafeArea(
        child: devices == null
            ? const SizedBox.shrink()
            : devices.isEmpty
            ? const Padding(
                padding: EdgeInsets.all(32),
                child: Text(
                  'No devices yet.\nOn a server run "talkeed pair", then scan its code with the '
                  'scan button on the main screen.',
                  style: TextStyle(color: Colors.white54, height: 1.5),
                ),
              )
            : ListView(
                children: [
                  for (final d in devices)
                    ListTile(
                      key: Key('device-${d.id}'),
                      leading: Icon(d.icon, color: Colors.white54),
                      title: Text(d.name),
                      subtitle: Text(
                        [d.kind, if (d.addr.isNotEmpty) d.addr].join(' · '),
                        style: const TextStyle(color: Colors.white38, fontSize: 12),
                      ),
                      trailing: _busy == d.id
                          ? const SizedBox(
                              width: 20,
                              height: 20,
                              child: CircularProgressIndicator(strokeWidth: 2),
                            )
                          : Row(
                              mainAxisSize: MainAxisSize.min,
                              children: [
                                TextButton(
                                  onPressed: _busy == null ? () => _status(d) : null,
                                  child: const Text('Status'),
                                ),
                                TextButton(
                                  onPressed: () => _forget(d),
                                  child: const Text('Forget'),
                                ),
                              ],
                            ),
                    ),
                ],
              ),
      ),
    );
  }
}
