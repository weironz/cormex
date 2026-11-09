import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// 记忆页的展示方式：卡片看内容，列表扫得快。
enum MemoryViewMode { cards, list }

/// 记住上次选的展示方式。存不进去（隐私窗口）就只在本次有效。
class MemoryView extends Notifier<MemoryViewMode> {
  static const _storage = FlutterSecureStorage();
  static const _key = 'memory_view_mode';
  bool _chosen = false;

  @override
  MemoryViewMode build() {
    _restore();
    return MemoryViewMode.cards;
  }

  Future<void> _restore() async {
    try {
      final saved = await _storage.read(key: _key);
      // 读回来之前人已经点过了：以人点的为准
      if (!ref.mounted || _chosen) return;
      state = MemoryViewMode.values.firstWhere(
        (m) => m.name == saved,
        orElse: () => MemoryViewMode.cards,
      );
    } catch (_) {}
  }

  Future<void> set(MemoryViewMode mode) async {
    _chosen = true;
    state = mode;
    try {
      await _storage.write(key: _key, value: mode.name);
    } catch (_) {}
  }
}

final memoryViewProvider = NotifierProvider<MemoryView, MemoryViewMode>(
  MemoryView.new,
);
