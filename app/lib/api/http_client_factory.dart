/// 平台缝之一。
///
/// 这个文件之上的一切（API 实现、controller、widget）都与平台无关，只管
/// 要一个 `http.Client`；只有这道条件导出后面的两个实现知道自己在哪个平台。
/// 这就是为什么 `lib/features/` 与 `lib/widgets/` 里一句 `kIsWeb` 都没有。
///
/// 为什么值得开这道缝：浏览器里 `package:http` 默认的 `BrowserClient` 建在
/// `XMLHttpRequest` 上，并且**不发送**跨源预检之外的那套 fetch 语义；
/// `fetch_client` 实现同一个 `http.Client` 接口，但走浏览器的 `fetch`，
/// CORS 模式与凭据策略都能显式说清楚 —— 而这个客户端天生就是要跨源去打
/// 一台别的机器上的 cormexd。
library;

export 'http_client_factory_io.dart'
    if (dart.library.js_interop) 'http_client_factory_web.dart';
