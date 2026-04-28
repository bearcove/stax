import SwiftUI

struct ContentView: View {
    @State private var model = AppModel()

    var body: some View {
        VStack(spacing: 0) {
            Toolbar(model: model)
            ZStack {
                Color(white: 0.10)
                Text("flame goes here")
                    .font(.system(.callout, design: .monospaced))
                    .foregroundStyle(.tertiary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .preferredColorScheme(model.lightMode ? .light : .dark)
    }
}

#Preview {
    ContentView()
        .frame(width: 1200, height: 700)
}
