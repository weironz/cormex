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

    // 纯路径 = 同源 + 前缀。这是 Web 镜像不必烧死域名的那个机制：
    // 换成构建期编译一个绝对地址也能跑通同一套部署，但发出去的镜像
    // 就只对我们这一个域名有用了。
    test('纯路径解析为同源加前缀', () {
      final base = CormexApi.resolveBase(
        '/api',
        Uri.parse('https://cormex.cloudcele.com/app/?x=1#frag'),
      );
      expect(
        base.toString(),
        'https://cormex.cloudcele.com/api',
        reason: '前缀要接在页面的源上，而页面自己的路径/查询串/锚点一概不要',
      );
    });

    test('纯路径在没有页面源时仍然保住前缀', () {
      expect(
        CormexApi.resolveBase('/api', Uri.parse('file:///C:/app/')).toString(),
        'http://127.0.0.1:8080/api',
        reason: '「API 挂在哪个前缀下」与「哪台机器」是两件事 —— '
            '回落换的是主机，不该把前缀一起丢掉',
      );
    });

    test('尾斜杠削干净后单独一个斜杠就是同源根', () {
      expect(
        CormexApi.resolveBase('/', Uri.parse('https://cormex.example.com/'))
            .toString(),
        'https://cormex.example.com',
        reason: '`/` 与空串是同一件事，不能变成一个尾巴上挂着斜杠的 base —— '
            '那之后每个请求的路径都会是 //health',
      );
    });

    // `_uri` 拼的是 `_base.path + path`。前缀那一支要是把 path 丢了，
    // 这里就会静默变成打根路径 —— 而 traefik 的 StripPrefix 只匹配
    // /api，于是请求直接落到 Web 那个 router 上，拿回一份 index.html。
    // 症状是「JSON 解析失败」，读起来完全不像路由配错。
    test('前缀 base 拼出来的请求路径带着前缀', () {
      final base = CormexApi.resolveBase(
        '/api',
        Uri.parse('https://cormex.cloudcele.com/'),
      );
      expect(
        base.replace(path: '${base.path}/health').toString(),
        'https://cormex.cloudcele.com/api/health',
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
