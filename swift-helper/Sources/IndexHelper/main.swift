import Foundation
import IndexStoreDB

// MARK: - Output Types

struct EdgeOutput: Codable {
    let from: String
    let to: String
}

struct IndexOutput: Codable {
    let edges: [EdgeOutput]
    let fileCount: Int
    let edgeCount: Int
}

// MARK: - Main

func main() throws {
    let args = CommandLine.arguments

    guard args.count >= 3 else {
        fputs("""
        Usage: index-helper <store-path> <database-path> [--lib-path <path>] [--repo-root <path>] [--files-from <path>]
        """, stderr)
        exit(1)
    }

    let storePath = args[1]
    let databasePath = args[2]

    var libPath: String?
    var repoRoot: String?
    var filesFromPath: String?

    var i = 3
    while i < args.count {
        switch args[i] {
        case "--lib-path":
            i += 1
            if i < args.count { libPath = args[i] }
        case "--repo-root":
            i += 1
            if i < args.count { repoRoot = args[i] }
        case "--files-from":
            i += 1
            if i < args.count { filesFromPath = args[i] }
        default:
            break
        }
        i += 1
    }

    if libPath == nil { libPath = findLibIndexStore() }

    guard let resolvedLibPath = libPath else {
        fputs("Error: Could not find libIndexStore.dylib\n", stderr)
        exit(1)
    }

    let repoRootSlash: String? = repoRoot.map { $0.hasSuffix("/") ? $0 : $0 + "/" }

    // Load project file set for filtering.
    var projectFiles: Set<String> = []
    if let filesPath = filesFromPath {
        let content = try String(contentsOfFile: filesPath, encoding: .utf8)
        for line in content.split(separator: "\n") {
            let rel = String(line)
            if !rel.isEmpty {
                projectFiles.insert(rel)
                // Also store full path version for matching.
                if let prefix = repoRootSlash {
                    projectFiles.insert(prefix + rel)
                }
            }
        }
    }

    fputs("Opening index store...\n", stderr)

    try FileManager.default.createDirectory(atPath: databasePath, withIntermediateDirectories: true)

    let lib = try IndexStoreLibrary(dylibPath: resolvedLibPath)
    let db = try IndexStoreDB(
        storePath: storePath,
        databasePath: databasePath,
        library: lib,
        listenToUnitEvents: false
    )
    db.pollForUnitChangesAndWait()

    let interestingKinds: Set<IndexSymbolKind> = [.class, .struct, .enum, .protocol, .typealias]
    let startTime = CFAbsoluteTimeGetCurrent()

    // Single pass: scan canonical symbols, collect only project-defined ones,
    // then query their occurrences for edges.

    // Step 1: collect USRs defined in project files.
    var projectUSRs: [String: String] = [:]  // USR → relative def file
    var scannedCount = 0
    var skippedCount = 0

    db.forEachCanonicalSymbolOccurrence(
        containing: "",
        anchorStart: false,
        anchorEnd: false,
        subsequence: false,
        ignoreCase: true
    ) { occurrence in
        scannedCount += 1

        guard interestingKinds.contains(occurrence.symbol.kind) else {
            return true
        }
        guard occurrence.roles.contains(.definition) else {
            return true
        }

        let filePath = occurrence.location.path
        let relPath = makeRelative(filePath, prefix: repoRootSlash)

        // Skip if not a project file.
        if !projectFiles.isEmpty && !projectFiles.contains(relPath) && !projectFiles.contains(filePath) {
            skippedCount += 1
            return true
        }

        projectUSRs[occurrence.symbol.usr] = relPath
        return true
    }

    let scanTime = CFAbsoluteTimeGetCurrent()
    fputs("Scan: \(scannedCount) symbols, \(projectUSRs.count) project defs, \(skippedCount) skipped in \(String(format: "%.1f", scanTime - startTime))s\n", stderr)

    // Step 2: for each project-defined USR, query references → build edges.
    var edgeSet: Set<String> = []
    var edges: [EdgeOutput] = []
    var filesInEdges: Set<String> = []

    for (usr, defFile) in projectUSRs {
        db.forEachSymbolOccurrence(byUSR: usr, roles: .reference) { occ in
            let refPath = makeRelative(occ.location.path, prefix: repoRootSlash)

            // Skip if not a project file or same file.
            guard refPath != defFile else { return true }
            if !projectFiles.isEmpty && !projectFiles.contains(refPath) && !projectFiles.contains(occ.location.path) {
                return true
            }

            let key = "\(defFile)\t\(refPath)"
            guard edgeSet.insert(key).inserted else { return true }

            edges.append(EdgeOutput(from: defFile, to: refPath))
            filesInEdges.insert(defFile)
            filesInEdges.insert(refPath)
            return true
        }
    }

    let totalTime = CFAbsoluteTimeGetCurrent()
    fputs("Edges: \(edges.count) across \(filesInEdges.count) files in \(String(format: "%.1f", totalTime - scanTime))s\n", stderr)
    fputs("Total: \(String(format: "%.1f", totalTime - startTime))s\n", stderr)

    let output = IndexOutput(edges: edges, fileCount: filesInEdges.count, edgeCount: edges.count)
    let data = try JSONEncoder().encode(output)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
}

// MARK: - Helpers

func makeRelative(_ path: String, prefix: String?) -> String {
    guard let p = prefix, path.hasPrefix(p) else { return path }
    return String(path.dropFirst(p.count))
}

func findLibIndexStore() -> String? {
    if let path = xcodeSelectPath(), FileManager.default.fileExists(atPath: path) { return path }
    if FileManager.default.fileExists(atPath: "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/libIndexStore.dylib") {
        return "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/libIndexStore.dylib"
    }
    if let apps = try? FileManager.default.contentsOfDirectory(atPath: "/Applications") {
        for app in apps where app.hasPrefix("Xcode") && app.hasSuffix(".app") {
            let p = "/Applications/\(app)/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/libIndexStore.dylib"
            if FileManager.default.fileExists(atPath: p) { return p }
        }
    }
    return nil
}

func xcodeSelectPath() -> String? {
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: "/usr/bin/xcode-select")
    proc.arguments = ["-p"]
    let pipe = Pipe()
    proc.standardOutput = pipe
    proc.standardError = FileHandle.nullDevice
    try? proc.run()
    proc.waitUntilExit()
    guard proc.terminationStatus == 0 else { return nil }
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    guard let dir = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) else { return nil }
    return "\(dir)/Toolchains/XcodeDefault.xctoolchain/usr/lib/libIndexStore.dylib"
}

do { try main() } catch { fputs("Error: \(error)\n", stderr); exit(1) }
