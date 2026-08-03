import Foundation

protocol Worker {
    func process(_ value: String) -> String
}

class Service: NSObject, Worker {
    func process(_ value: String) -> String {
        value.trimmingCharacters(in: .whitespaces)
    }
}

class Runner: Service {
    func execute(_ value: String) -> String {
        process(value)
    }
}
