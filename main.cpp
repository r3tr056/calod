#include <iostream>
#include <cstring>
#include <cstdlib>
#include <unistd.h>
#include <arpa/inet.h>

int main(int, char**){
    int serverSocket = socket(AF_INET, SOCK_STREAM, 0);
    if (serverSocket == -1) {
        std::cerr << "Error creating socket" << std::endl;
        return EXIT_FAILURE;
    }

    sockaddr_in serverAddress;
    serverAddress.sin_family = AF_INET;
    serverAddress.sin_port = htons(8080);
    serverAddress.sin_addr.s_addr = INADDR_ANY;

    // Bind the socket to the specified address and port
    if (bind(serverSocket, (struct socketaddr*)&serverAddress, sizeof(serverAddress)) == -1) {
        std::cerr << "Error binding socket" << std::endl;
        close(serverSocket);
        return EXIT_FAILURE;
    }

    // Listen for incomming connectings
    if (listen(serverSocket, 10) == -1) {
        std::cerr << "Error listening for connections" << std::endl;
        close(serverSocket);
        return EXIT_FAILURE;
    }

    std::cout << "Server is listening on port 8080..." << std::endl;

    while (true) {
        int clientSocket = accept(serverSocket, nullptr, nullptr);
        if (clientSocket == -1) {
            std::cerr << "Error accepting connections" << std::endl;
            close(serverSocket);
            return EXIT_FAILURE;
        }

        const char* welcomeMessage = "Welcome to Redis Server!\n";
        send(clientSocket, welcomeMessage, strlen(welcomeMessage), 0);

        close(clientSocket)
    }

    close(serverSocket);

    return EXIT_SUCCESS;
}
