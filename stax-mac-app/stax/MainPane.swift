import SwiftUI

struct MainPane: View {
    @Bindable var model: AppModel

    var body: some View {
        VSplitView {
            topPane
                .frame(minHeight: 200)

            HSplitView {
                FunctionTable(model: model)
                    .frame(minWidth: 320)
                DetailTabs(model: model)
                    .frame(minWidth: 320)
            }
            .frame(minHeight: 160)
        }
    }

    @ViewBuilder
    private var topPane: some View {
        if let fn = focusedFunction {
            VStack(spacing: 0) {
                NavHeader(model: model, focused: fn)
                Divider()
                CallGraphView(model: model, focused: fn)
            }
        } else {
            VStack(spacing: 0) {
                minimap
                Divider()
                flame
            }
        }
    }

    private var focusedFunction: AppModel.FunctionEntry? {
        guard let id = model.focusedFunctionId else { return nil }
        return model.functions.first { $0.id == id }
    }

    private var minimap: some View {
        Minimap(timeline: model.timeline)
            .frame(height: 56)
    }

    private var flame: some View {
        FlameView(flamegraph: model.flamegraph)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// Stacked-tree flame graph. Each level is one fixed-height row;
/// children of a node are placed left-to-right under their parent,
/// with widths proportional to their `on_cpu_ns`. Coloured by the
/// node's language string.
private struct FlameView: View {
    let flamegraph: FlamegraphUpdate?

    var body: some View {
        ZStack {
            Color(nsColor: .textBackgroundColor).opacity(0.3)
            if let flamegraph {
                Canvas { ctx, size in
                    let totalNs = max(1, flamegraph.root.onCpuNs)
                    drawFlameNode(
                        flamegraph.root,
                        in: ctx,
                        rect: CGRect(origin: .zero, size: size),
                        depth: 0,
                        totalNs: totalNs,
                        strings: flamegraph.strings
                    )
                }
            } else {
                Text("waiting for flamegraph…")
                    .font(.mono(.callout))
                    .foregroundStyle(.tertiary)
            }
        }
    }
}

private let flameLevelHeight: CGFloat = 18
private let flameMaxDepth = 32

private func drawFlameNode(
    _ node: FlameNode,
    in ctx: GraphicsContext,
    rect: CGRect,
    depth: Int,
    totalNs: UInt64,
    strings: [String]
) {
    if depth >= flameMaxDepth { return }
    if rect.width < 1 { return }

    let y = CGFloat(depth) * flameLevelHeight
    let cell = CGRect(x: rect.minX, y: y, width: rect.width, height: flameLevelHeight - 1)
    let fill = flameNodeColor(node, strings: strings)
    ctx.fill(Path(cell), with: .color(fill))

    if cell.width >= 32 {
        let name = stringAt(node.functionName, in: strings) ?? hex(node.address)
        let text = Text(name)
            .font(.mono(.caption2))
            .foregroundStyle(.primary)
        let labelRect = cell.insetBy(dx: 4, dy: 1)
        ctx.draw(text, in: labelRect)
    }

    // Recurse into children stacked horizontally under us.
    let parentNs = max(1, node.onCpuNs)
    var childX = rect.minX
    for child in node.children {
        let childWidth = rect.width * CGFloat(child.onCpuNs) / CGFloat(parentNs)
        if childWidth >= 0.5 {
            drawFlameNode(
                child,
                in: ctx,
                rect: CGRect(x: childX, y: y, width: childWidth, height: rect.height),
                depth: depth + 1,
                totalNs: totalNs,
                strings: strings
            )
        }
        childX += childWidth
    }
}

private func stringAt(_ idx: UInt32?, in strings: [String]) -> String? {
    guard let i = idx, Int(i) < strings.count else { return nil }
    return strings[Int(i)]
}

private func hex(_ addr: UInt64) -> String {
    String(format: "0x%llx", addr)
}

private func flameNodeColor(_ node: FlameNode, strings: [String]) -> Color {
    let lang = (Int(node.language) < strings.count ? strings[Int(node.language)] : "")
        .lowercased()
    switch lang {
    case "rust":           return Color(red: 0.74, green: 0.56, blue: 0.91).opacity(0.6)
    case "c":              return Color(red: 0.36, green: 0.78, blue: 0.85).opacity(0.6)
    case "cpp", "c++":     return Color(red: 0.36, green: 0.65, blue: 0.95).opacity(0.6)
    case "swift":          return Color(red: 0.95, green: 0.55, blue: 0.43).opacity(0.6)
    case "objc",
         "objective-c",
         "objectivec":     return Color(red: 0.55, green: 0.82, blue: 0.45).opacity(0.6)
    default:               return Color(red: 0.96, green: 0.78, blue: 0.27).opacity(0.6)
    }
}

private struct Minimap: View {
    let timeline: TimelineUpdate?

    var body: some View {
        ZStack {
            Color(nsColor: .underPageBackgroundColor)
            if let timeline, !timeline.buckets.isEmpty {
                Canvas { ctx, size in
                    drawTimeline(timeline, in: ctx, size: size)
                }
            } else {
                Text("waiting for timeline…")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    /// Each bucket renders as two stacked columns: on-CPU (green) at
    /// the bottom, off-CPU (gray) above it. Bar height is fraction of
    /// the bucket-window the thread group spent in that state.
    private func drawTimeline(
        _ timeline: TimelineUpdate,
        in ctx: GraphicsContext,
        size: CGSize
    ) {
        guard !timeline.buckets.isEmpty else { return }
        let bucketCount = timeline.buckets.count
        let bucketWidth = size.width / CGFloat(bucketCount)
        let bucketSizeNs = max(1, Double(timeline.bucketSizeNs))

        // Cap at the bucket size — saturated buckets reach the top.
        let onColor = Color.green.opacity(0.7)
        let offColor = Color.gray.opacity(0.45)

        for (i, bucket) in timeline.buckets.enumerated() {
            let x = CGFloat(i) * bucketWidth
            let onRatio = min(1, Double(bucket.onCpuNs) / bucketSizeNs)
            let offRatio = min(1, Double(bucket.offCpuNs) / bucketSizeNs)

            let onHeight = CGFloat(onRatio) * size.height
            let offHeight = CGFloat(offRatio) * (size.height - onHeight)

            let onRect = CGRect(
                x: x,
                y: size.height - onHeight,
                width: max(1, bucketWidth - 0.5),
                height: onHeight
            )
            let offRect = CGRect(
                x: x,
                y: size.height - onHeight - offHeight,
                width: max(1, bucketWidth - 0.5),
                height: offHeight
            )
            ctx.fill(Path(onRect), with: .color(onColor))
            ctx.fill(Path(offRect), with: .color(offColor))
        }
    }
}

private struct NavHeader: View {
    @Bindable var model: AppModel
    let focused: AppModel.FunctionEntry

    var body: some View {
        HStack(spacing: 8) {
            Button {
                model.focusedFunctionId = nil
            } label: {
                HStack(spacing: 3) {
                    Image(systemName: "chevron.left")
                    Text("flame")
                }
                .font(.caption)
            }
            .buttonStyle(.plain)
            .help("Back to flame graph")

            Image(systemName: "chevron.right")
                .font(.caption2)
                .foregroundStyle(.tertiary)

            LanguageBadge(kind: focused.kind, size: 12)
            Text(focused.name)
                .font(.mono(.caption))
                .lineLimit(1)
            Text(focused.binary)
                .font(.mono(.caption))
                .foregroundStyle(.tertiary)
                .lineLimit(1)

            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
    }
}
