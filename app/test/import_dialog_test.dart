import 'dart:convert';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/sessions/import_dialog.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

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
}
