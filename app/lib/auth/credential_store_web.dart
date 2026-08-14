import 'package:web/web.dart' as web;

import 'stored_credential.dart';

/// 浏览器标签页没有环境。留着这个名字只为让缝的两侧暴露同一组符号。
const String kTokenEnvVar = 'CORTEXD_TOKEN';

const bool kCanRememberCredential = true;

const String _credentialKey = 'cormex.credential';

/// 地址不是秘密，所以它可以进 `localStorage`（跨标签页、留到明天）。
/// 凭据不行 —— 见下面。
const String _baseUrlKey = 'cormex.base_url';

/// 显示在连接表单下面。**把暴露面直说**，而不是暗示浏览器给得出它给不出的
/// 那种安全。
///
/// 这个区分是真实的、也值得多写一句：把「记住」读成「安全存储」的人，会做出
/// 与把它读成「本页上任何脚本都读得到」的人**不同的部署决定**（比如把
/// cortexd 直接挂到公网上）。
const String kCredentialStorageNote =
    '浏览器里没有安全的凭据存储：sessionStorage 与 localStorage 一样，'
    '会被本页面上运行的任何脚本读到。这里只用 sessionStorage —— '
    '它随标签页关闭而消失，刷新页面还在，但不会跨标签页、也不会留到明天。';

/// Web 上「记住的」与「种子」是同一处，所以这是 [readSeedCredential] 的
/// 异步壳。留着这个名字是为了让上层**一行 `kIsWeb` 都不用写**。
Future<StoredCredential?> readCredential() async => readSeedCredential();

StoredCredential? readSeedCredential() {
  try {
    return StoredCredential.decode(
      web.window.sessionStorage.getItem(_credentialKey),
    );
  } on Object {
    // 沙箱 iframe、或者浏览器被配置成禁止站点数据 —— 两种都会在
    // **取属性**这一步就抛，而不是在取值那一步。当作「什么都没存」处理，
    // 否则一个隐私设置就会让应用启动时白屏。
    return null;
  }
}

Future<void> writeCredential(StoredCredential credential) async {
  try {
    web.window.sessionStorage.setItem(_credentialKey, credential.encode());
  } on Object {
    // 存储被挡住了。这次会话照样能用，静静地只留在内存里，比让一次刚刚
    // 成功的连接失败要好。
  }
}

Future<void> clearCredential() async {
  try {
    web.window.sessionStorage.removeItem(_credentialKey);
  } on Object {
    // 之前那次写入要是失败了，本来就什么都没存。
  }
}

Future<String?> readBaseUrl() async {
  try {
    final v = web.window.localStorage.getItem(_baseUrlKey);
    return (v == null || v.isEmpty) ? null : v;
  } on Object {
    return null;
  }
}

Future<void> writeBaseUrl(String baseUrl) async {
  try {
    web.window.localStorage.setItem(_baseUrlKey, baseUrl);
  } on Object {
    // 同上：记不住地址只是下次要重填一行。
  }
}
