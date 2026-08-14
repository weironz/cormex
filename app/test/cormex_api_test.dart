import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/auth/stored_credential.dart';
import 'package:cormex_app/models/memory_search_result.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('resolveBase', () {
    // 空串在浏览器里是**有意义的配置**（同源反代），不是「没填」。
    // 把它顶成一个默认地址，会让反代那套拓扑整个走不通，而症状是
    // 「Failed to fetch」—— 一句看起来像后端没起来的错。
    test('空地址在有页面源时解析为同源', () {
      final base = CormexApi.resolveBase(
        '',
        Uri.parse('https://memory.example.com/app/?x=1#frag'),
      );
      expect(
        base.toString(),
        'https://memory.example.com',
        reason: '同源解析必须只保留 scheme+host+port —— '
            '页面的路径、查询串、锚点都不属于 API 根，留着它们之后每个请求都会带上',
      );
    });

    test('空地址在没有页面源时回落到本机 daemon', () {
      final base = CormexApi.resolveBase('', Uri.parse('file:///C:/app/'));
      expect(
        base.toString(),
        'http://127.0.0.1:8080',
        reason: '桌面端没有同源可言，那儿的空串才真是「没配」',
      );
    });

    test('补齐 scheme 并去掉尾斜杠', () {
      expect(
        CormexApi.resolveBase('memory.example.com/api/', Uri.base).toString(),
        'http://memory.example.com/api',
      );
    });
  });

  group('StoredCredential', () {
    test('往返之后 kind 还在', () {
      const c = StoredCredential(kind: CredentialKind.refresh, value: 'abc');
      final back = StoredCredential.decode(c.encode());
      expect(
        back?.kind,
        CredentialKind.refresh,
        reason: 'kind 丢了的话，启动时就只能靠「先当 bearer 试一次，401 了再当 '
            'refresh 试」来猜，而猜错那一半会在服务端留下一条失败的认证记录',
      );
      expect(back?.value, 'abc');
    });

    test('坏数据当作「没存过」，不抛', () {
      expect(StoredCredential.decode('not json'), isNull);
      expect(StoredCredential.decode('{"kind":"bearer"}'), isNull);
      expect(StoredCredential.decode(null), isNull);
    });

    test('toString 不泄露凭据', () {
      const c = StoredCredential(kind: CredentialKind.bearer, value: 's3cr3t');
      expect(
        c.toString(),
        isNot(contains('s3cr3t')),
        reason: 'toString 会进日志与错误消息',
      );
    });
  });

  group('MemorySearchResult', () {
    test('解析 facts 与按 fact id 索引的召回路', () {
      final r = MemorySearchResult.fromJson({
        'facts': [
          {'id': 'f1', 'statement': '偏好 Rust', 'confidence': 0.92},
          {'id': 'f2', 'statement': '住在杭州', 'invalidated': true},
        ],
        'channels': [
          {
            'fact_id': 'f1',
            'channels': ['bm25', 'vector'],
            'score': 0.7,
          },
        ],
      });

      expect(r.facts.length, 2);
      expect(r.channels['f1']?.channels, ['bm25', 'vector']);
      expect(
        r.invalidatedCount,
        1,
        reason: '回放路径靠这个数字告诉用户「这一屏里有几条当时成立、现在已被推翻」—— '
            '不带 as_of 时它恒为 0，因为那条路查的是 active_facts 视图',
      );
    });

    test('缺字段不抛：这个界面要对着比它新和比它旧的服务端都活下来', () {
      final r = MemorySearchResult.fromJson({'facts': null});
      expect(r.isEmpty, isTrue);
      expect(r.channels, isEmpty);
    });
  });
}
