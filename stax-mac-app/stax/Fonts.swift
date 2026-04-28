import SwiftUI

extension Font {
    /// Maple Mono at a size that scales with the given Dynamic Type style.
    static func mono(_ style: Font.TextStyle = .body) -> Font {
        .custom("Maple Mono", size: monoSize(for: style), relativeTo: style)
    }

    /// Maple Mono at a fixed point size (no Dynamic Type scaling).
    static func mono(fixed size: CGFloat) -> Font {
        .custom("Maple Mono", fixedSize: size)
    }
}

private func monoSize(for style: Font.TextStyle) -> CGFloat {
    switch style {
    case .largeTitle:  26
    case .title:       22
    case .title2:      17
    case .title3:      15
    case .headline:    13
    case .body:        13
    case .callout:     12
    case .subheadline: 11
    case .footnote:    10
    case .caption:     10
    case .caption2:     9
    @unknown default:  13
    }
}
