import 'dart:io';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';

import 'stored_credential.dart';

/// `cortexd --generate-token` 告诉运维去设的那个环境变量。
///
/// 名字**照抄**：客户端另起一个名字，会让 daemon 自己印出来的那行说明变成
/// 错的。
const String kTokenEnvVar = 'CORTEXD_TOKEN';

/// 桌面端能记住凭据。
const bool kCanRememberCredential = true;

/// 显示在连接表单下面。
const String kCredentialStorageNote =
    '凭据保存在系统凭据管理器里（Windows 凭据管理器 / macOS 钥匙串 / libsecret），'
    '不是明文文件。下次打开不用再填；点「断开」会立刻清掉。';

/// Windows / Linux / macOS 的默认实现已经是 DPAPI / libsecret / Keychain，
/// 不需要额外配置。刻意**不**写 Android 的选项：那个参数在 v10 里已经废弃，
/// 写上去只会留下一条编译警告和一个将来会失效的承诺。
const _storage = FlutterSecureStorage();

/// 带应用前缀：系统凭据库是**全机共享**的命名空间，一个叫 `credential` 的
/// 键迟早会跟别的软件撞上。
const String _credentialKey = 'cormex.credential';
const String _baseUrlKey = 'cormex.base_url';

/// 不需要用户输入任何东西就能拿到的凭据。
///
/// **顺序有意义**：记住的那把优先于环境变量。连过一次的人期待自己还连着
/// 同一个身份，而某人 shell profile 里一条陈旧的 `CORTEXD_TOKEN` 不该把他
/// 悄悄换回单用户那条老路。
Future<StoredCredential?> readCredential() async {
  try {
    final stored = StoredCredential.decode(
      await _storage.read(key: _credentialKey),
    );
    if (stored != null) return stored;
  } on Object catch (_) {
    // 钥匙串锁着、libsecret 没跑、条目损坏 —— 都落在这里。没有一种该让
    // 应用起不来，它们的意思都只是「你得重新连一次」。
  }
  return readSeedCredential();
}

/// 环境变量里的预共享 token —— 自托管部署的常态。
StoredCredential? readSeedCredential() {
  final raw = Platform.environment[kTokenEnvVar]?.trim();
  if (raw == null || raw.isEmpty) return null;
  return StoredCredential(kind: CredentialKind.bearer, value: raw);
}

Future<void> writeCredential(StoredCredential credential) async {
  try {
    await _storage.write(key: _credentialKey, value: credential.encode());
  } on Object catch (e) {
    // 存不进去不等于连接失败 —— 内存里这次会话好好的。**要说出来**，
    // 否则用户会在下次打开时才发现又要重填，而那时已经没人知道为什么。
    // ignore: avoid_print
    print('凭据没能存进系统凭据库（下次打开需要重新填）：$e');
  }
}

Future<void> clearCredential() async {
  try {
    await _storage.delete(key: _credentialKey);
  } on Object catch (_) {
    // 删一个本来就不在的键，或者够不着钥匙串。两种都没有后续动作。
  }
}

/// 后端地址不是秘密，但跟凭据放同一处：桌面端没有第二个「配置」存储，
/// 为了一个字符串再引一个包不划算。
Future<String?> readBaseUrl() async {
  try {
    final v = await _storage.read(key: _baseUrlKey);
    return (v == null || v.isEmpty) ? null : v;
  } on Object catch (_) {
    return null;
  }
}

Future<void> writeBaseUrl(String baseUrl) async {
  try {
    await _storage.write(key: _baseUrlKey, value: baseUrl);
  } on Object catch (_) {
    // 记不住地址只是下次要重填一行，不值得打断任何事。
  }
}
