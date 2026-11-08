import 'dart:convert';
import 'dart:typed_data';

import 'package:cormex_app/features/sessions/export_slimmer.dart';
import 'package:flutter_test/flutter_test.dart';

/// A zip of stored entries, written by hand: the test must not lean on the
/// code under test to build its input. [streamed] entries are written the way
/// a streaming zipper writes them: zero sizes in the local header, flag bit 3,
/// and a data descriptor after the bytes.
Uint8List zipOf(
  List<(String, List<int>)> files, {
  Set<String> streamed = const {},
}) {
  final out = BytesBuilder();
  final central = BytesBuilder();
  for (final (name, body) in files) {
    final n = utf8.encode(name);
    final offset = out.length;
    final stream = streamed.contains(name);
    const crc = 0x12345678; // carried, not checked: entries are copied as is
    final local = ByteData(30)
      ..setUint32(0, 0x04034b50, Endian.little)
      ..setUint16(4, 20, Endian.little)
      ..setUint16(6, stream ? 0x0808 : 0x0800, Endian.little)
      ..setUint32(14, stream ? 0 : crc, Endian.little)
      ..setUint32(18, stream ? 0 : body.length, Endian.little)
      ..setUint32(22, stream ? 0 : body.length, Endian.little)
      ..setUint16(26, n.length, Endian.little);
    out
      ..add(local.buffer.asUint8List())
      ..add(n)
      ..add(body);
    if (stream) {
      final d = ByteData(16)
        ..setUint32(0, 0x08074b50, Endian.little)
        ..setUint32(4, crc, Endian.little)
        ..setUint32(8, body.length, Endian.little)
        ..setUint32(12, body.length, Endian.little);
      out.add(d.buffer.asUint8List());
    }
    final c = ByteData(46)
      ..setUint32(0, 0x02014b50, Endian.little)
      ..setUint16(4, 20, Endian.little)
      ..setUint16(6, 20, Endian.little)
      ..setUint16(8, stream ? 0x0808 : 0x0800, Endian.little)
      ..setUint32(16, crc, Endian.little)
      ..setUint32(20, body.length, Endian.little)
      ..setUint32(24, body.length, Endian.little)
      ..setUint16(28, n.length, Endian.little)
      ..setUint32(42, offset, Endian.little);
    central
      ..add(c.buffer.asUint8List())
      ..add(n);
  }
  final cd = central.takeBytes();
  final end = ByteData(22)
    ..setUint32(0, 0x06054b50, Endian.little)
    ..setUint16(8, files.length, Endian.little)
    ..setUint16(10, files.length, Endian.little)
    ..setUint32(12, cd.length, Endian.little)
    ..setUint32(16, out.length, Endian.little);
  out
    ..add(cd)
    ..add(end.buffer.asUint8List());
  return out.takeBytes();
}

/// Read a stored zip back: name → bytes, through the central directory.
Map<String, List<int>> entriesOf(Uint8List zip) {
  final d = ByteData.sublistView(zip);
  var eocd = zip.length - 22;
  while (d.getUint32(eocd, Endian.little) != 0x06054b50) {
    eocd--;
  }
  final out = <String, List<int>>{};
  var p = d.getUint32(eocd + 16, Endian.little);
  for (var i = 0; i < d.getUint16(eocd + 10, Endian.little); i++) {
    final size = d.getUint32(p + 20, Endian.little);
    final nameLength = d.getUint16(p + 28, Endian.little);
    final name = utf8.decode(zip.sublist(p + 46, p + 46 + nameLength));
    final local = d.getUint32(p + 42, Endian.little);
    expect(
      d.getUint32(local + 18, Endian.little),
      size,
      reason: '$name: the local header must carry the size, not a descriptor',
    );
    expect(d.getUint16(local + 6, Endian.little) & 0x0008, 0, reason: name);
    final start =
        local +
        30 +
        d.getUint16(local + 26, Endian.little) +
        d.getUint16(local + 28, Endian.little);
    out[name] = zip.sublist(start, start + size);
    p +=
        46 +
        nameLength +
        d.getUint16(p + 30, Endian.little) +
        d.getUint16(p + 32, Endian.little);
  }
  return out;
}

void main() {
  test('only the conversations leave the zip, byte for byte', () {
    final page0 = utf8.encode('[{"title":"标题"}]');
    final page1 = utf8.encode('[{"title":"two"}]');
    final zip = zipOf(
      [
        ('conversations-000.json', page0),
        ('file-abc.dat', List.filled(5000, 7)),
        ('export_manifest.json', utf8.encode('{}')),
        ('conversations-001.json', page1),
        ('dalle-generations/img.webp', List.filled(5000, 9)),
      ],
      streamed: {'conversations-001.json'},
    );

    final slim = slimExportZip(zip)!;
    expect(slim.kept, ['conversations-000.json', 'conversations-001.json']);
    expect(slim.total, 5);
    expect(
      slim.bytes.length,
      lessThan(zip.length ~/ 10),
      reason: 'attachments stay behind',
    );
    final back = entriesOf(slim.bytes);
    expect(back['conversations-000.json'], page0);
    expect(
      back['conversations-001.json'],
      page1,
      reason: 'an entry with a data descriptor is copied with its real size',
    );
  });

  test('every name the server reads is kept, and nothing else', () {
    for (final name in [
      'conversations.json',
      'export/conversations.json',
      'conversations-022.json',
      'conversations-000.zip',
      'prod-grok-backend.json',
      'Takeout/My Activity/Gemini Apps/MyActivity.json',
    ]) {
      expect(isConversationEntry(name), isTrue, reason: name);
    }
    for (final name in [
      'conversation_asset_file_names.json',
      'group_chats.json',
      'memories-000.zip',
      'conversations-.json',
      'Takeout/My Activity/YouTube/MyActivity.json',
      'file-conversations-000.json.dat',
    ]) {
      expect(isConversationEntry(name), isFalse, reason: name);
    }
  });

  test('a zip without conversations is refused with what it holds', () {
    final zip = zipOf([
      ('photos/1.jpg', [1]),
    ]);
    expect(
      () => slimExportZip(zip),
      throwsA(
        isA<NoConversationsInZip>().having((e) => e.names, 'names', [
          'photos/1.jpg',
        ]),
      ),
    );
  });

  test('a plain JSON file is not touched', () {
    expect(slimExportZip(Uint8List.fromList(utf8.encode('[1]'))), isNull);
  });
}
