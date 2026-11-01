import 'package:cormex_app/state/reading_preferences.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('阅读宽度可保存并在新实例恢复', () async {
    TestWidgetsFlutterBinding.ensureInitialized();
    FlutterSecureStorage.setMockInitialValues({});
    final first = ProviderContainer();
    first.read(readingPreferencesProvider);
    expect(
      await first.read(readingPreferencesProvider.notifier).setWidth(1100),
      isTrue,
    );
    first.dispose();
    final second = ProviderContainer();
    addTearDown(second.dispose);
    second.read(readingPreferencesProvider);
    await Future<void>.delayed(Duration.zero);
    expect(second.read(readingPreferencesProvider), 1100);
  });
}
