import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../state/connection_controller.dart';

class DeviceAuthorizePage extends ConsumerStatefulWidget {
  const DeviceAuthorizePage({super.key});

  @override
  ConsumerState<DeviceAuthorizePage> createState() =>
      _DeviceAuthorizePageState();
}

class _DeviceAuthorizePageState extends ConsumerState<DeviceAuthorizePage> {
  final code = TextEditingController();
  Map<String, dynamic>? request;
  String? inspectedCode;
  String? error;
  bool busy = false;
  bool approved = false;

  @override
  void dispose() {
    code.dispose();
    super.dispose();
  }

  Future<void> inspect() async {
    final value = code.text.trim().toUpperCase();
    setState(() {
      busy = true;
      error = null;
      request = null;
      inspectedCode = null;
    });
    try {
      final result = await ref.read(apiProvider).inspectDevice(value);
      if (mounted) {
        setState(() {
          request = result;
          inspectedCode = value;
        });
      }
    } on Object catch (e) {
      if (mounted) setState(() => error = '$e');
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> confirm() async {
    if (inspectedCode == null ||
        inspectedCode != code.text.trim().toUpperCase()) {
      return;
    }
    setState(() {
      busy = true;
      error = null;
    });
    try {
      await ref.read(apiProvider).confirmDevice(inspectedCode!);
      if (mounted) setState(() => approved = true);
    } on Object catch (e) {
      if (mounted) setState(() => error = '$e');
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('授权设备')),
    body: Center(
      child: SingleChildScrollView(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 480),
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                if (approved)
                  const Text('设备已授权，可以返回终端。')
                else ...[
                  const Text('输入终端显示的设备码，查看设备和请求的权限。'),
                  const SizedBox(height: 16),
                  TextField(
                    controller: code,
                    enabled: !busy,
                    decoration: const InputDecoration(
                      labelText: '设备码',
                      hintText: 'ABCD-EFGH',
                    ),
                    onChanged: (_) => setState(() {
                      request = null;
                      inspectedCode = null;
                    }),
                  ),
                  const SizedBox(height: 16),
                  OutlinedButton(
                    onPressed: busy ? null : inspect,
                    child: const Text('查看请求'),
                  ),
                  if (request case final data?) ...[
                    const SizedBox(height: 20),
                    Text('设备：${data['label']}'),
                    if (data['platform'] != null)
                      Text('平台：${data['platform']}'),
                    Text('请求权限：${(data['scopes'] as List).join('、')}'),
                    const SizedBox(height: 12),
                    const Text('请仅授权你本人发起的请求。'),
                    FilledButton(
                      onPressed: busy ? null : confirm,
                      child: const Text('确认授权'),
                    ),
                  ],
                ],
                if (error != null)
                  Padding(
                    padding: const EdgeInsets.only(top: 16),
                    child: Text(
                      error!,
                      style: TextStyle(
                        color: Theme.of(context).colorScheme.error,
                      ),
                    ),
                  ),
              ],
            ),
          ),
        ),
      ),
    ),
  );
}
