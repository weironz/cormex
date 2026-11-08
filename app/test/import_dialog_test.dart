import 'dart:convert';
import 'dart:typed_data';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/sessions/export_slimmer.dart';
import 'package:cormex_app/features/sessions/import_dialog.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

import 'export_slimmer_test.dart' show zipOf, entriesOf;

void main() {
  testWidgets('an export is previewed, then imported only after confirming', (
    tester,
  ) async {
    final paths = <String>[];
    final c = ProviderContainer(
      overrides: [
        exportPickerProvider.overrideWithValue(
          () async =>
              PickedExport(name: 'chatgpt.zip', bytes: utf8.encode('x')),
        ),
        apiProvider.overrideWithValue(
          CormexApi(
            baseUrl: 'http://localhost',
            client: MockClient((r) async {
              paths.add(r.url.path);
              if (r.url.path.endsWith('/import/upload')) {
                return http.Response('{"handle":"h1","size_bytes":1}', 200);
              }
              final run = r.url.path.endsWith('/run');
              return http.Response(
                jsonEncode({
                  'agent': 'chatgpt_export',
                  'source': 'ChatGPT',
                  'conversations': 5,
                  'new': 2,
                  'changed': 1,
                  'unchanged': 2,
                  'written': run,
                }),
                200,
                headers: {'content-type': 'application/json; charset=utf-8'},
              );
            }),
          ),
        ),
      ],
    );
    addTearDown(c.dispose);
    await tester.pumpWidget(
      UncontrolledProviderScope(
        container: c,
        child: const MaterialApp(home: Scaffold(body: ImportArchiveDialog())),
      ),
    );

    expect(
      find.text('Grok'),
      findsOneWidget,
      reason: 'says where each export comes from',
    );
    await tester.tap(find.text('选择文件'));
    await tester.pumpAndSettle();
    expect(find.textContaining('ChatGPT 导出，共 5 段对话'), findsOneWidget);
    expect(
      paths.where((p) => p.endsWith('/run')),
      isEmpty,
      reason: 'choosing a file previews; nothing is written before confirming',
    );

    await tester.tap(find.text('导入 3 段'));
    await tester.pumpAndSettle();
    expect(paths.last, endsWith('/archive/import/run'));
    expect(find.text('已导入 ChatGPT 的聊天'), findsOneWidget);
  });

  testWidgets('a file with nothing new cannot be imported again', (
    tester,
  ) async {
    final c = ProviderContainer(
      overrides: [
        exportPickerProvider.overrideWithValue(
          () async => PickedExport(name: 'same.zip', bytes: utf8.encode('x')),
        ),
        apiProvider.overrideWithValue(
          CormexApi(
            baseUrl: 'http://localhost',
            client: MockClient((r) async {
              if (r.url.path.endsWith('/import/upload')) {
                return http.Response('{"handle":"h1"}', 200);
              }
              return http.Response(
                '{"agent":"claude_export","source":"Claude","conversations":4,'
                '"new":0,"changed":0,"unchanged":4,"written":false}',
                200,
              );
            }),
          ),
        ),
      ],
    );
    addTearDown(c.dispose);
    await tester.pumpWidget(
      UncontrolledProviderScope(
        container: c,
        child: const MaterialApp(home: Scaffold(body: ImportArchiveDialog())),
      ),
    );
    await tester.tap(find.text('选择文件'));
    await tester.pumpAndSettle();
    final button = tester.widget<FilledButton>(
      find.widgetWithText(FilledButton, '导入 0 段'),
    );
    expect(
      button.onPressed,
      isNull,
      reason: 'everything is already archived; there is nothing to write',
    );
  });

  testWidgets('a whole vendor zip uploads only its conversations', (
    tester,
  ) async {
    final zip = zipOf([
      ('conversations-000.json', utf8.encode('[{"id":"a"}]')),
      ('file-1.dat', List.filled(200000, 1)),
      ('file-2.dat', List.filled(200000, 2)),
    ]);
    List<int>? uploaded;
    final c = ProviderContainer(
      overrides: [
        exportPickerProvider.overrideWithValue(
          () async => PickedExport(name: 'openai.zip', bytes: zip),
        ),
        apiProvider.overrideWithValue(
          CormexApi(
            baseUrl: 'http://localhost',
            client: MockClient((r) async {
              if (r.url.path.endsWith('/import/upload')) {
                uploaded = r.bodyBytes;
                return http.Response('{"handle":"h1","size_bytes":1}', 200);
              }
              return http.Response(
                jsonEncode({
                  'agent': 'chatgpt_export',
                  'source': 'ChatGPT',
                  'conversations': 1,
                  'new': 1,
                  'changed': 0,
                  'unchanged': 0,
                  'written': false,
                }),
                200,
                headers: {'content-type': 'application/json; charset=utf-8'},
              );
            }),
          ),
        ),
      ],
    );
    addTearDown(c.dispose);
    await tester.pumpWidget(
      UncontrolledProviderScope(
        container: c,
        child: const MaterialApp(home: Scaffold(body: ImportArchiveDialog())),
      ),
    );
    await tester.tap(find.text('选择文件'));
    await tester.pumpAndSettle();

    expect(uploaded, isNotNull);
    expect(
      uploaded!.length,
      lessThan(zip.length ~/ 100),
      reason: 'the attachments must not be uploaded',
    );
    expect(entriesOf(Uint8List.fromList(uploaded!)).keys, [
      'conversations-000.json',
    ]);
    expect(find.textContaining('ChatGPT 导出，共 1 段对话'), findsOneWidget);
  });

  testWidgets(
    'a zip with no conversations says what it holds, uploads nothing',
    (tester) async {
      var calls = 0;
      final c = ProviderContainer(
        overrides: [
          exportPickerProvider.overrideWithValue(
            () async => PickedExport(
              name: 'photos.zip',
              bytes: zipOf([
                ('photos/1.jpg', [1]),
              ]),
            ),
          ),
          apiProvider.overrideWithValue(
            CormexApi(
              baseUrl: 'http://localhost',
              client: MockClient((r) async {
                calls++;
                return http.Response('{}', 500);
              }),
            ),
          ),
        ],
      );
      addTearDown(c.dispose);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: c,
          child: const MaterialApp(home: Scaffold(body: ImportArchiveDialog())),
        ),
      );
      await tester.tap(find.text('选择文件'));
      await tester.pumpAndSettle();
      expect(find.textContaining('photos/1.jpg'), findsOneWidget);
      expect(calls, 0);
      expect(const NoConversationsInZip([]), isA<Exception>());
    },
  );
}
