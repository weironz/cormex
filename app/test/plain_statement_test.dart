import 'package:cormex_app/core/formatting.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('plainStatement', () {
    test('旧审计报告压成一句纯文本', () {
      const raw =
          '**来源报告**\n\n## MSYS 路径转换\n\n'
          '本次需禁用路径转换，否则会被改写。\n\n'
          '## 适用场景\n\n该 Windows 宿主。';
      final plain = plainStatement(raw);
      expect(plain.contains('**'), isFalse, reason: plain);
      expect(plain.contains('\n'), isFalse, reason: plain);
      expect(plain.contains('##'), isFalse, reason: plain);
      expect(plain.contains('助手自述，未独立核实'), isTrue, reason: plain);
      expect(plain.contains('MSYS 路径转换'), isTrue, reason: plain);
    });

    test('新格式的一句话原样保留', () {
      const s = 'Cortex 项目的对象存储已改用 RustFS，不再使用 MinIO。';
      expect(plainStatement(s), s);
    });

    test('去掉标题与列表标记，保留行内代码', () {
      const raw = '## 主题\n\n- 保留 `runc` 回滚\n- 不改默认 runtime';
      expect(plainStatement(raw), '主题 保留 `runc` 回滚 不改默认 runtime');
    });
  });
}
