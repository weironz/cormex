/// 构建期钉进来的配置。
///
/// 现在只有一个旋钮，但它值得单独一个文件：`String.fromEnvironment` 是
/// **编译期常量折叠**，散落在使用点上时没人看得出「这个值运行期改不了」。
abstract final class AppConfig {
  /// 用户没配过地址时，连接页里预填的那个。
  ///
  /// 空串 = 同源（浏览器）或本机 daemon（桌面端），见
  /// [CormexApi.resolveBase]。Web 镜像把它定成 `/api`：同源，但 API 挂在
  /// `/api` 前缀后面 —— 与镜像里 nginx 之外那层反代（traefik / nginx）
  /// 的 StripPrefix 对上。
  ///
  /// **这里刻意不写任何域名。** 写了就意味着 Web 镜像只对我们这一个部署
  /// 有用，而这个仓库是要开源的。路径前缀是同源相对的，谁的域名都对。
  ///
  /// 桌面端不传这个 define，于是保持空串。
  static const String defaultBaseUrl = String.fromEnvironment(
    'CORMEX_BASE_URL',
  );
}
