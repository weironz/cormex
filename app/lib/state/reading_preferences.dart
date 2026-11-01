import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

class ReadingPreferences extends Notifier<double> {
  static const _storage = FlutterSecureStorage();
  static const _key = 'conversation_width';
  int _revision = 0;

  @override
  double build() {
    _restore();
    return 880;
  }

  Future<void> _restore() async {
    final revision = _revision;
    try {
      final value = double.tryParse(await _storage.read(key: _key) ?? '');
      if (ref.mounted &&
          revision == _revision &&
          value != null &&
          value.isFinite) {
        state = value.clamp(560, 1400);
      }
    } catch (_) {
      // Storage can be unavailable in private browsing.
    }
  }

  void preview(double value) {
    _revision++;
    state = value.clamp(560, 1400);
  }

  Future<bool> setWidth(double value) async {
    preview(value);
    try {
      await _storage.write(key: _key, value: state.toString());
      return true;
    } catch (_) {
      return false;
    }
  }
}

final readingPreferencesProvider = NotifierProvider<ReadingPreferences, double>(
  ReadingPreferences.new,
);
