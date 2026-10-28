# 研究ノート：ガウス分布

平均 μ、分散 σ² の**正規分布**の確率密度関数は次の式で表される。

$$ f(x) = \frac{1}{\sqrt{2\pi\sigma^2}} \exp\left( -\frac{(x-\mu)^2}{2\sigma^2} \right) $$

全区間で積分すると1になることは、ガウス積分から確かめられる。

$$ \int_{-\infty}^{\infty} e^{-x^2} \mathrm{d}x = \sqrt{\pi} $$

| 区間 | 含まれる確率 | メモ |
|---|---:|---|
| μ ± σ | 68.3% | 約3分の2 |
| μ ± 2σ | 95.4% | 外れ値の目安 |
| μ ± 3σ | 99.7% | 3σルール 🎯 |

```rust
fn gaussian(x: f64, mu: f64, sigma: f64) -> f64 {
    let z = (x - mu) / sigma;
    (-0.5 * z * z).exp() / (sigma * (2.0 * std::f64::consts::PI).sqrt())
}
```
