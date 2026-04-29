import SwiftUI

/// 2D call graph view backed by `NeighborsUpdate`. The focused
/// function sits at depth 0 in the middle; callers fan upward from
/// `callersTree`, callees fan downward from `calleesTree`. Each
/// subtree gets a tidy-tree layout (Reingold-Tilford simplified):
/// each leaf claims one column, each internal node sits centered
/// above its children. Edges are bezier curves between parent and
/// child rectangles.
struct CallGraphView: View {
    @Bindable var model: AppModel
    let focused: AppModel.FunctionEntry

    var body: some View {
        ZStack {
            Color(nsColor: .textBackgroundColor).opacity(0.3)
            if let neighbors = model.neighbors {
                graph(for: neighbors)
            } else {
                Text("waiting for neighbors…")
                    .font(.mono(.callout))
                    .foregroundStyle(.tertiary)
            }
        }
    }

    @ViewBuilder
    private func graph(for neighbors: NeighborsUpdate) -> some View {
        let layout = CallGraphLayout.compute(neighbors: neighbors)
        let bounds = layout.bounds(
            nodeWidth: nodeWidth,
            nodeHeight: nodeHeight,
            colGap: colGap,
            rowGap: rowGap,
            padding: padding
        )

        ScrollView([.horizontal, .vertical]) {
            ZStack(alignment: .topLeading) {
                Canvas { ctx, _ in
                    for edge in layout.edges {
                        drawCallEdge(
                            from: rectFor(layout.nodes[edge.from], bounds: bounds),
                            to: rectFor(layout.nodes[edge.to], bounds: bounds),
                            in: ctx
                        )
                    }
                }
                .frame(width: bounds.size.width, height: bounds.size.height)

                ForEach(layout.nodes) { node in
                    CallNodeView(
                        node: node,
                        strings: neighbors.strings,
                        maxOnCpu: layout.maxOnCpu
                    )
                    .frame(width: nodeWidth, height: nodeHeight)
                    .position(
                        x: rectFor(node, bounds: bounds).midX,
                        y: rectFor(node, bounds: bounds).midY
                    )
                }
            }
            .frame(width: bounds.size.width, height: bounds.size.height)
        }
    }

    private func rectFor(_ node: CallGraphNode, bounds: CallGraphBounds) -> CGRect {
        let xPx = padding
            + CGFloat(node.col - bounds.minCol) * (nodeWidth + colGap)
        let yPx = padding
            + CGFloat(node.depth - bounds.minDepth) * (nodeHeight + rowGap)
        return CGRect(x: xPx, y: yPx, width: nodeWidth, height: nodeHeight)
    }

    // Layout knobs — kept on the view so the renderer + bounds
    // computation share them.
    private let nodeWidth: CGFloat = 220
    private let nodeHeight: CGFloat = 36
    private let colGap: CGFloat = 16
    private let rowGap: CGFloat = 36
    private let padding: CGFloat = 24
}

// MARK: - Layout

struct CallGraphNode: Identifiable {
    let id: Int
    let role: CallGraphRole
    let address: UInt64
    let functionName: UInt32?
    let binary: UInt32?
    let language: UInt32
    let onCpuNs: UInt64
    let petSamples: UInt64
    /// Tidy-tree column in leaf units (signed; the focused node's col
    /// becomes the origin after re-centering).
    let col: Int
    /// Negative = above focused (caller direction); positive = below
    /// (callee direction); 0 = focused.
    let depth: Int
}

enum CallGraphRole { case caller, focused, callee }

struct CallGraphEdge {
    let from: Int  // index into nodes[]
    let to: Int
}

struct CallGraphLayout {
    var nodes: [CallGraphNode] = []
    var edges: [CallGraphEdge] = []
    var maxOnCpu: UInt64 = 1

    static func compute(neighbors: NeighborsUpdate) -> CallGraphLayout {
        var layout = CallGraphLayout()

        // Walk callers (depth grows negative → upward) and callees
        // (depth grows positive → downward) using the simplified
        // tidy-tree algorithm: each leaf claims one column, each
        // internal node centers above its children.
        var callersCols: [Int] = []
        var calleesCols: [Int] = []

        let callersFocusedId = layout.appendTreeWalk(
            neighbors.callersTree,
            depthSign: -1,
            startCol: 0,
            childRole: .caller,
            colsOut: &callersCols
        )
        let calleesFocusedId = layout.appendTreeWalk(
            neighbors.calleesTree,
            depthSign: +1,
            startCol: 0,
            childRole: .callee,
            colsOut: &calleesCols
        )

        // Re-center: align both focused-roots onto column 0 and drop
        // one of the duplicates from the rendered set (they're the
        // same function — the call graph should show focused once).
        let callersFocusedCol = layout.nodes[callersFocusedId].col
        let calleesFocusedCol = layout.nodes[calleesFocusedId].col

        var rebuilt: [CallGraphNode] = []
        rebuilt.reserveCapacity(layout.nodes.count - 1)

        // Map old index → new index (for edge rewriting). The dropped
        // duplicate (calleesFocusedId) is remapped to callersFocusedId.
        var remap = [Int: Int]()
        for node in layout.nodes {
            if node.id == calleesFocusedId {
                remap[node.id] = remap[callersFocusedId] ?? callersFocusedId
                continue
            }
            let shift = node.depth >= 0 ? (callersFocusedCol - calleesFocusedCol) : 0
            let recentered = CallGraphNode(
                id: rebuilt.count,
                role: node.role,
                address: node.address,
                functionName: node.functionName,
                binary: node.binary,
                language: node.language,
                onCpuNs: node.onCpuNs,
                petSamples: node.petSamples,
                col: node.col + shift - callersFocusedCol,
                depth: node.depth
            )
            remap[node.id] = rebuilt.count
            rebuilt.append(recentered)
        }
        layout.nodes = rebuilt
        layout.edges = layout.edges.compactMap { e in
            guard let from = remap[e.from], let to = remap[e.to] else {
                return nil
            }
            if from == to { return nil }
            return CallGraphEdge(from: from, to: to)
        }
        layout.maxOnCpu = max(1, layout.nodes.map(\.onCpuNs).max() ?? 1)
        return layout
    }

    /// Recursively walk a FlameNode subtree, appending nodes in
    /// post-order (children first) and recording parent→child edges.
    /// Returns the index of `root` in `self.nodes` so callers can
    /// reconstruct the focused-root's column afterwards.
    @discardableResult
    private mutating func appendTreeWalk(
        _ root: FlameNode,
        depthSign: Int,
        startCol: Int,
        childRole: CallGraphRole,
        colsOut: inout [Int]
    ) -> Int {
        var leafCursor = startCol

        func walk(
            _ node: FlameNode,
            depth: Int,
            isRoot: Bool,
            layout: inout CallGraphLayout
        ) -> Int {
            let role: CallGraphRole = isRoot ? .focused : childRole

            if node.children.isEmpty {
                let col = leafCursor
                leafCursor += 1
                let id = layout.nodes.count
                layout.nodes.append(
                    CallGraphNode(
                        id: id,
                        role: role,
                        address: node.address,
                        functionName: node.functionName,
                        binary: node.binary,
                        language: node.language,
                        onCpuNs: node.onCpuNs,
                        petSamples: node.petSamples,
                        col: col,
                        depth: depth * depthSign
                    )
                )
                return id
            }

            var childIds: [Int] = []
            for child in node.children {
                let childId = walk(child, depth: depth + 1, isRoot: false, layout: &layout)
                childIds.append(childId)
            }
            let centerCol =
                (layout.nodes[childIds.first!].col + layout.nodes[childIds.last!].col) / 2
            let id = layout.nodes.count
            layout.nodes.append(
                CallGraphNode(
                    id: id,
                    role: role,
                    address: node.address,
                    functionName: node.functionName,
                    binary: node.binary,
                    language: node.language,
                    onCpuNs: node.onCpuNs,
                    petSamples: node.petSamples,
                    col: centerCol,
                    depth: depth * depthSign
                )
            )
            for c in childIds {
                // Edge direction: in the rendered graph, callers'
                // edges point from caller (deeper-up) into focused
                // (shallower-up); for callees the opposite. We always
                // emit child→root in tree-walk order; the edge
                // renderer treats `from` as parent, `to` as child
                // visually, so reverse here for callers.
                if depthSign < 0 {
                    layout.edges.append(CallGraphEdge(from: c, to: id))
                } else {
                    layout.edges.append(CallGraphEdge(from: id, to: c))
                }
            }
            return id
        }

        let rootId = walk(root, depth: 0, isRoot: true, layout: &self)
        colsOut.append(self.nodes[rootId].col)
        return rootId
    }
}

struct CallGraphBounds {
    var minCol: Int
    var maxCol: Int
    var minDepth: Int
    var maxDepth: Int
    var size: CGSize
}

extension CallGraphLayout {
    func bounds(
        nodeWidth: CGFloat,
        nodeHeight: CGFloat,
        colGap: CGFloat,
        rowGap: CGFloat,
        padding: CGFloat
    ) -> CallGraphBounds {
        let cols = nodes.map(\.col)
        let depths = nodes.map(\.depth)
        let minCol = cols.min() ?? 0
        let maxCol = cols.max() ?? 0
        let minDepth = depths.min() ?? 0
        let maxDepth = depths.max() ?? 0
        let width =
            padding * 2
            + CGFloat(maxCol - minCol + 1) * nodeWidth
            + CGFloat(maxCol - minCol) * colGap
        let height =
            padding * 2
            + CGFloat(maxDepth - minDepth + 1) * nodeHeight
            + CGFloat(maxDepth - minDepth) * rowGap
        return CallGraphBounds(
            minCol: minCol,
            maxCol: maxCol,
            minDepth: minDepth,
            maxDepth: maxDepth,
            size: CGSize(width: max(width, 320), height: max(height, 200))
        )
    }
}

// MARK: - Node view + edge renderer

private struct CallNodeView: View {
    let node: CallGraphNode
    let strings: [String]
    let maxOnCpu: UInt64

    var body: some View {
        HStack(spacing: 6) {
            LanguageBadge(kind: kind, size: 12)
            Text(displayName)
                .font(.mono(.caption))
                .foregroundStyle(node.role == .focused ? .primary : .secondary)
                .lineLimit(1)
            Spacer(minLength: 4)
            BarTrack(ratio: ratio)
                .frame(width: 36, height: 3)
            Text(formatDuration(TimeInterval(node.onCpuNs) / 1_000_000_000))
                .font(.mono(.caption2))
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 4)
        .background(background)
        .overlay {
            RoundedRectangle(cornerRadius: 4)
                .stroke(borderColor, lineWidth: node.role == .focused ? 1 : 0.5)
        }
        .clipShape(.rect(cornerRadius: 4))
    }

    private var ratio: Double {
        Double(node.onCpuNs) / Double(maxOnCpu)
    }

    private var displayName: String {
        if let i = node.functionName, Int(i) < strings.count {
            return strings[Int(i)]
        }
        return String(format: "0x%llx", node.address)
    }

    private var kind: SymbolKind {
        let lang = Int(node.language) < strings.count ? strings[Int(node.language)] : ""
        switch lang.lowercased() {
        case "rust":   return .rust
        case "c":      return .c
        case "cpp", "c++": return .cpp
        case "swift":  return .swift
        case "objc", "objective-c", "objectivec": return .objc
        default:       return .unknown
        }
    }

    private var background: Color {
        switch node.role {
        case .focused: Color.accentColor.opacity(0.18)
        default:       Color(nsColor: .textBackgroundColor)
        }
    }

    private var borderColor: Color {
        switch node.role {
        case .focused: Color.accentColor.opacity(0.7)
        default:       Color.secondary.opacity(0.4)
        }
    }
}

private func drawCallEdge(from: CGRect, to: CGRect, in ctx: GraphicsContext) {
    // Always draw from "above" rect to "below" rect — direction is
    // determined by the layout (caller edges already swap so the
    // arrow visually descends from caller into focused, etc.).
    let (top, bottom) = from.midY < to.midY ? (from, to) : (to, from)
    let s = CGPoint(x: top.midX, y: top.maxY)
    let e = CGPoint(x: bottom.midX, y: bottom.minY)
    let dy = e.y - s.y
    let cp1 = CGPoint(x: s.x, y: s.y + dy * 0.4)
    let cp2 = CGPoint(x: e.x, y: e.y - dy * 0.4)

    var path = Path()
    path.move(to: s)
    path.addCurve(to: e, control1: cp1, control2: cp2)
    ctx.stroke(path, with: .color(.secondary.opacity(0.6)), lineWidth: 1)

    // Arrowhead pointing into the lower rect.
    var arrow = Path()
    arrow.move(to: e)
    arrow.addLine(to: CGPoint(x: e.x - 4, y: e.y - 6))
    arrow.addLine(to: CGPoint(x: e.x + 4, y: e.y - 6))
    arrow.closeSubpath()
    ctx.fill(arrow, with: .color(.secondary.opacity(0.7)))
}
