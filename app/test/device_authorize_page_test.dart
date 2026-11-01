import 'dart:convert';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/connect/device_authorize_page.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

void main() {
  testWidgets('device request must be inspected before approval', (
    tester,
  ) async {
    final calls = <String>[];
    final client = MockClient((request) async {
      calls.add(request.url.path);
      expect(jsonDecode(request.body)['user_code'], 'ABCD-EFGH');
      if (request.url.path.endsWith('/inspect')) {
        return http.Response(
          jsonEncode({
            'label': 'my laptop',
            'platform': 'windows',
            'scopes': ['capture:write'],
          }),
          200,
        );
      }
      return http.Response('{"approved":true}', 200);
    });
    final api = CormexApi(
      baseUrl: 'https://example.test/api',
      token: 'test',
      client: client,
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [apiProvider.overrideWithValue(api)],
        child: const MaterialApp(home: DeviceAuthorizePage()),
      ),
    );
    expect(find.text('确认授权'), findsNothing);
    await tester.enterText(find.byType(TextField), 'abcd-efgh');
    await tester.tap(find.text('查看请求'));
    await tester.pumpAndSettle();
    expect(find.text('设备：my laptop'), findsOneWidget);
    expect(find.textContaining('capture:write'), findsOneWidget);
    expect(calls, ['/api/devices/inspect']);
    await tester.tap(find.text('确认授权'));
    await tester.pumpAndSettle();
    expect(find.text('设备已授权，可以返回终端。'), findsOneWidget);
    expect(calls, ['/api/devices/inspect', '/api/devices/confirm']);
  });
}
