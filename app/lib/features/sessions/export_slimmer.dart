import 'dart:convert';
import 'dart:typed_data';

/// 从厂商寄来的整包 zip 里只取出聊天记录，在浏览器里重新打成一个小 zip。
///
/// 为什么在浏览器里做：ChatGPT 的导出把图片和附件也放进同一个 zip（实测 275 MB，
/// 其中聊天只占 21 MB），整包上传既慢又会撞上服务端的上传上限，而附件根本不导入。
/// 只挑出聊天文件，附件就不必离开本机。
///
/// 为什么不用解压再压缩：直接拷贝每个条目**压缩后的原始字节**，只重写 zip 的目录结构。
/// 不解压、不重算 CRC，所以 100 MB 以上的聊天 JSON 也只是一次内存拷贝。
///
/// 挑哪些文件与服务端 `cormex_import::export_json` 认的名字一致；两边要一起改。
class SlimmedExport {
  const SlimmedExport({
    required this.bytes,
    required this.kept,
    required this.total,
  });

  /// 新 zip 的字节。
  final Uint8List bytes;

  /// 留下的条目名。
  final List<String> kept;

  /// 原 zip 里一共多少个条目。
  final int total;
}

/// zip 里没有任何聊天记录文件。
class NoConversationsInZip implements Exception {
  const NoConversationsInZip(this.names);

  /// 原 zip 里的条目名（最多前 20 个），让用户看清自己选了什么。
  final List<String> names;

  @override
  String toString() => 'zip 里没有聊天记录文件；它包含：${names.join('、')}';
}

/// 服务端认的聊天文件名。
bool isConversationEntry(String name) {
  final lower = name.toLowerCase();
  final base = lower.split('/').last;
  if (base == 'conversations.json' || base == 'prod-grok-backend.json') {
    return true;
  }
  if (base == 'myactivity.json' &&
      (lower.contains('gemini') || lower.contains('bard'))) {
    return true;
  }
  // ChatGPT 2026-11 起的分页：conversations-000.json …；
  // Claude 2026-11 起的分包：conversations-000.zip
  return RegExp(r'^conversations-\d+\.(json|zip)$').hasMatch(base);
}

const _eocdSig = 0x06054b50;
const _centralSig = 0x02014b50;
const _localSig = 0x04034b50;

/// 不是 zip、或是本函数不处理的 zip（ZIP64、分卷）时返回 `null`：调用方原样上传，
/// 由服务端去读。zip 里没有聊天文件时抛 [NoConversationsInZip]。
SlimmedExport? slimExportZip(Uint8List bytes) {
  final data = ByteData.sublistView(bytes);
  if (bytes.length < 22 || data.getUint32(0, Endian.little) != _localSig) {
    return null;
  }

  // 目录结尾记录在文件最后，后面最多跟 65535 字节的注释
  var eocd = -1;
  final lowest = bytes.length - 22 - 0xffff;
  for (var i = bytes.length - 22; i >= 0 && i >= lowest; i--) {
    if (data.getUint32(i, Endian.little) == _eocdSig) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) return null;
  final count = data.getUint16(eocd + 10, Endian.little);
  final centralOffset = data.getUint32(eocd + 16, Endian.little);
  final disk = data.getUint16(eocd + 4, Endian.little);
  // 0xffff / 0xffffffff 是 ZIP64 的占位：真实值在另一处记录里。这种包交给服务端
  if (count == 0xffff || centralOffset == 0xffffffff || disk != 0) return null;

  final names = <String>[];
  final kept = <({int central, String name, Uint8List nameBytes})>[];
  var p = centralOffset;
  for (var n = 0; n < count; n++) {
    if (p + 46 > bytes.length ||
        data.getUint32(p, Endian.little) != _centralSig) {
      return null;
    }
    final nameLength = data.getUint16(p + 28, Endian.little);
    final extraLength = data.getUint16(p + 30, Endian.little);
    final commentLength = data.getUint16(p + 32, Endian.little);
    final nameBytes = bytes.sublist(p + 46, p + 46 + nameLength);
    final name = utf8.decode(nameBytes, allowMalformed: true);
    names.add(name);
    if (isConversationEntry(name)) {
      if (data.getUint32(p + 20, Endian.little) == 0xffffffff ||
          data.getUint32(p + 24, Endian.little) == 0xffffffff ||
          data.getUint32(p + 42, Endian.little) == 0xffffffff) {
        return null;
      }
      kept.add((central: p, name: name, nameBytes: nameBytes));
    }
    p += 46 + nameLength + extraLength + commentLength;
  }
  if (kept.isEmpty) {
    throw NoConversationsInZip(names.take(20).toList());
  }

  final out = BytesBuilder(copy: false);
  final central = BytesBuilder(copy: false);
  var offset = 0;
  for (final entry in kept) {
    final c = entry.central;
    final local = data.getUint32(c + 42, Endian.little);
    if (local + 30 > bytes.length ||
        data.getUint32(local, Endian.little) != _localSig) {
      return null;
    }
    final compressed = data.getUint32(c + 20, Endian.little);
    final start =
        local +
        30 +
        data.getUint16(local + 26, Endian.little) +
        data.getUint16(local + 28, Endian.little);
    if (start + compressed > bytes.length) return null;
    // 大小和 CRC 一律取自中央目录：本地头里它们可能是 0（第 3 位标志 =
    // 数据后面另有描述符）。新包把值写进本地头并清掉这个标志，就不必拷贝描述符
    final flags = data.getUint16(c + 8, Endian.little) & ~0x0008;
    final nameLength = entry.nameBytes.length;

    final header = ByteData(30);
    header
      ..setUint32(0, _localSig, Endian.little)
      ..setUint16(4, data.getUint16(c + 6, Endian.little), Endian.little)
      ..setUint16(6, flags, Endian.little)
      ..setUint16(8, data.getUint16(c + 10, Endian.little), Endian.little)
      ..setUint16(10, data.getUint16(c + 12, Endian.little), Endian.little)
      ..setUint16(12, data.getUint16(c + 14, Endian.little), Endian.little)
      ..setUint32(14, data.getUint32(c + 16, Endian.little), Endian.little)
      ..setUint32(18, compressed, Endian.little)
      ..setUint32(22, data.getUint32(c + 24, Endian.little), Endian.little)
      ..setUint16(26, nameLength, Endian.little)
      ..setUint16(28, 0, Endian.little);
    out
      ..add(header.buffer.asUint8List())
      ..add(entry.nameBytes)
      ..add(Uint8List.sublistView(bytes, start, start + compressed));

    final record = ByteData(46);
    record
      ..setUint32(0, _centralSig, Endian.little)
      ..setUint16(4, data.getUint16(c + 4, Endian.little), Endian.little)
      ..setUint16(6, data.getUint16(c + 6, Endian.little), Endian.little)
      ..setUint16(8, flags, Endian.little)
      ..setUint16(10, data.getUint16(c + 10, Endian.little), Endian.little)
      ..setUint16(12, data.getUint16(c + 12, Endian.little), Endian.little)
      ..setUint16(14, data.getUint16(c + 14, Endian.little), Endian.little)
      ..setUint32(16, data.getUint32(c + 16, Endian.little), Endian.little)
      ..setUint32(20, compressed, Endian.little)
      ..setUint32(24, data.getUint32(c + 24, Endian.little), Endian.little)
      ..setUint16(28, nameLength, Endian.little)
      ..setUint16(30, 0, Endian.little)
      ..setUint16(32, 0, Endian.little)
      ..setUint16(34, 0, Endian.little)
      ..setUint16(36, data.getUint16(c + 36, Endian.little), Endian.little)
      ..setUint32(38, data.getUint32(c + 38, Endian.little), Endian.little)
      ..setUint32(42, offset, Endian.little);
    central
      ..add(record.buffer.asUint8List())
      ..add(entry.nameBytes);
    offset += 30 + nameLength + compressed;
  }
  final centralBytes = central.takeBytes();
  final end = ByteData(22);
  end
    ..setUint32(0, _eocdSig, Endian.little)
    ..setUint16(8, kept.length, Endian.little)
    ..setUint16(10, kept.length, Endian.little)
    ..setUint32(12, centralBytes.length, Endian.little)
    ..setUint32(16, offset, Endian.little);
  out
    ..add(centralBytes)
    ..add(end.buffer.asUint8List());
  return SlimmedExport(
    bytes: out.takeBytes(),
    kept: [for (final e in kept) e.name],
    total: count,
  );
}
