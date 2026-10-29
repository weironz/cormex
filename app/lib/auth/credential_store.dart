/// 平台缝之二：凭据存在哪。
///
/// # 为什么这道缝躲不掉
///
/// 存的是一把 **bearer 凭据**：拿着它的人就拥有整个记忆库（见
/// `cormexd::auth`）。它能躺在哪儿因此是个安全问题，而两个平台上的诚实答案
/// 是**真的不同** —— 不是「一个有 API 另一个没有」那么简单：
///
/// * **桌面端**有操作系统的凭据库（Windows 凭据管理器 / macOS 钥匙串 /
///   libsecret）：静态加密、按系统用户隔离、同机的另一个用户读不到。
/// * **Web 没有**。浏览器标签页能持久化的一切 —— `localStorage`、
///   `sessionStorage`、IndexedDB、JS 读得到的 cookie —— 都能被同源上运行的
///   **任何**脚本读到。没有哪种浏览器端存储真的能保护这个值，所以那边的
///   选择只关乎**暴露持续多久**。
///
/// # 刻意不做的两件事
///
/// * **没有 `--dart-define=CORMEX_TOKEN`。** 编译期常量会被烤进产物里。
///   Web 上那个产物是 `main.dart.js`，发给每一个打开页面的人 —— 令牌会随之
///   分发给每个访客。方便且灾难，所以这扇门连桌面端也不开：一个在一个平台
///   上好用、在另一个平台上泄密的构建开关，就是留给下一个跑
///   `flutter build web` 的人的陷阱。
/// * **桌面端不落明文文件。** 没有系统钥匙串托底的话，它严格劣于环境变量：
///   同样的暴露面，外加一个比用户记忆活得更久的文件。
///
/// 这个文件之上的一切只问 [kCanRememberCredential] 一次然后照着渲染。
/// 没有 widget import `dart:io`，没有 widget 碰 `package:web`，
/// 也没有 widget 查 `kIsWeb`。
library;

export 'credential_store_io.dart'
    if (dart.library.js_interop) 'credential_store_web.dart';
